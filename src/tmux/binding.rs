//! A missing socket is not evidence that an earlier tmux server stopped.
//! Reserve the dispatch before new-session. The host records the server it
//! actually runs under before spawning the worker, even if the client reply is lost.
use super::{failure, Reply, Target, Tmux, EXCLUSIVE_WINDOW};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub(super) enum Claim {
    LegacyPending {
        execution: Option<String>,
        socket: Option<PathBuf>,
        #[serde(flatten)]
        hints: LegacyHints,
    },
    Released(Option<String>),
    Launch(String),
    Registered {
        id: String,
        execution: String,
    },
    Observed {
        execution: Option<String>,
        server: ServerSession,
        token: String,
        #[serde(flatten)]
        hints: LegacyHints,
        receipt: Option<String>,
    },
}

// Persist the first trusted pane hint even when the socket is offline. A
// process first seen on reconnect must already have existed at that hint.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
enum PaneEvidence {
    Hint {
        boot: String,
        namespace: u64,
        listener: u64,
        ticks: u64,
    },
    Bound(PaneProcess),
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub(super) struct PaneProcess {
    pub(super) pid: u32,
    ticks: u64,
}

impl PaneProcess {
    pub(super) fn live(&self) -> bool {
        process_start(self.pid).is_ok_and(|ticks| ticks == Some(self.ticks))
    }
}

fn listener_inode(line: &[u8], socket: &Path) -> Option<u64> {
    // The seven numeric columns are ASCII. Paths may contain spaces or
    // non-UTF-8 bytes, and the kernel pads short inode numbers with spaces.
    let (columns, address) = line.split_at(line.iter().position(|b| *b == b'/')?);
    if address != socket.as_os_str().as_encoded_bytes() {
        return None;
    }
    let mut fields = std::str::from_utf8(columns).ok()?.split_ascii_whitespace();
    if fields.nth(3)? != "00010000" || fields.next()? != "0001" || fields.next()? != "01" {
        return None;
    }
    let inode = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some(inode)
}

impl PaneEvidence {
    fn hint(t: &Tmux<'_>) -> Option<Self> {
        // The kernel retains a listening socket's bound name across rename.
        // Require one stream listener for the historical address, even if its
        // filesystem entry is temporarily inaccessible.
        let socket = match &t.socket {
            Some(path) => path.clone(),
            None => {
                let out = t
                    .run(&[
                        "-N",
                        "display-message",
                        "-p",
                        "-t",
                        &format!("={}:", t.session),
                        "#{socket_path}",
                    ])
                    .ok()?;
                PathBuf::from(std::str::from_utf8(&out.stdout).ok()?.strip_suffix('\n')?)
            }
        };
        let sockets = fs::read("/proc/net/unix").ok()?;
        let mut listeners = sockets
            .split(|b| *b == b'\n')
            .filter_map(|line| listener_inode(line, &socket));
        let listener = listeners.next()?;
        if listeners.next().is_some() {
            return None;
        }
        // The request's CLI process was born before parsing, waiting on the
        // dispatch lock or locating the socket. A same-tick birth is ambiguous.
        let ticks = process_start(std::process::id()).ok()??;
        Some(Self::Hint {
            boot: boot_id().ok()?,
            namespace: fs::metadata("/proc/self/ns/pid").ok()?.ino(),
            listener,
            ticks,
        })
    }

    fn verify_server(&self, server: &ServerSession) -> Reply<()> {
        let Self::Hint {
            boot,
            namespace,
            listener,
            ..
        } = self
        else {
            return Ok(()); // A bound process is covered by its stored ServerSession.
        };
        let expected = PathBuf::from(format!("socket:[{listener}]"));
        if boot == &server.boot
            && *namespace == server.namespace
            && fs::read_dir(format!("/proc/{}/fd", server.pid)).is_ok_and(|fds| {
                fds.flatten()
                    .any(|fd| fs::read_link(fd.path()).is_ok_and(|path| path == expected))
            })
        {
            Ok(())
        } else {
            Err(unknown())
        }
    }

    fn matches(&self, process: &PaneProcess) -> bool {
        match self {
            Self::Bound(original) => original == process,
            Self::Hint { ticks, .. } => process.ticks < *ticks,
        }
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct LegacyHints {
    pane: Option<String>,
    launch: Option<String>,
    evidence: Option<PaneEvidence>,
}

impl LegacyHints {
    fn expected_launch<'a>(&'a self, execution: Option<&'a str>) -> Option<&'a str> {
        self.launch
            .as_deref()
            .or_else(|| execution.filter(|id| valid_launch_id(id)))
    }

    fn lookup(&self, t: &Tmux<'_>) -> Self {
        if t.legacy_worker_pane.is_none() && t.legacy_launch_id.is_none() {
            return self.clone();
        }
        // Probe the new hints on their own; an unverified stale hint must
        // neither hide a verified match nor authorize a pane-only fallback.
        Self {
            evidence: self
                .evidence
                .clone()
                .filter(|_| self.pane.as_deref() == t.legacy_worker_pane),
            pane: t.legacy_worker_pane.map(str::to_owned),
            launch: t.legacy_launch_id.map(str::to_owned),
        }
    }

    fn merge(&mut self, t: &Tmux<'_>) -> Reply<()> {
        if self.pane.is_none() && t.legacy_worker_pane.is_some() {
            self.evidence = PaneEvidence::hint(t);
        }
        for (saved, supplied) in [
            (&mut self.pane, t.legacy_worker_pane),
            (&mut self.launch, t.legacy_launch_id),
        ] {
            if let Some(value) = supplied {
                if saved.as_deref().is_some_and(|old| old != value) {
                    return Err(unknown());
                }
                *saved = Some(value.to_owned());
            }
        }
        Ok(())
    }
}

impl Claim {
    pub(super) fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Launch(id) | Self::Registered { id, .. } => Some(id),
            Self::Observed {
                execution, token, ..
            } => Some(execution.as_deref().unwrap_or(token)),
            Self::Released(id) | Self::LegacyPending { execution: id, .. } => id.as_deref(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ServerSession {
    #[serde(rename = "launchId", skip_serializing_if = "Option::is_none")]
    launch_id: Option<String>,
    socket: PathBuf,
    boot: String,
    namespace: u64,
    pid: u32,
    start_ticks: u64,
    session: String,
    pane: Option<String>,
}

pub(super) fn valid_pane(id: &str) -> bool {
    id.strip_prefix('%')
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

pub(super) fn valid_launch_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
}

pub(super) fn host_launch_id(
    pid: u32,
    state: &Path,
    dispatch: &str,
    target: &Target,
) -> Option<String> {
    // A pane can move. The R1 supervisor inherited its original dispatch and
    // tmux server/session at startup, independently of its current window.
    let environment = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let field = |prefix: &[u8]| {
        environment
            .split(|b| *b == 0)
            .find_map(|v| v.strip_prefix(prefix))
    };
    let session = target.session.strip_prefix('$')?;
    let endpoint = format!(",{},{session}", target.server_pid);
    if field(b"WFC_STATE=")? != state.as_os_str().as_encoded_bytes()
        || field(b"WFC_DISPATCH=")? != dispatch.as_bytes()
        || !field(b"TMUX=")?.ends_with(endpoint.as_bytes())
    {
        return None;
    }
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<_> = cmd.strip_suffix(&[0])?.split(|c| *c == 0).collect();
    let [_, b"--tmux-host", id] = args.as_slice() else {
        return None;
    };
    let id = std::str::from_utf8(id)
        .ok()
        .filter(|s| valid_launch_id(s))?;
    let receipt = super::launch::read_receipt(&state.join("launches").join(format!("{id}.json")))?;
    let child = u32::try_from(receipt["pid"].as_u64()?).ok()?;
    let stat = fs::read_to_string(format!("/proc/{child}/stat")).ok()?;
    let mut fields = stat.rsplit_once(") ")?.1.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    let parent: u32 = fields.next()?.parse().ok()?;
    (receipt["category"] == "started" && parent == pid).then(|| id.to_owned())
}

pub(super) fn unknown() -> serde_json::Value {
    failure(
        "uncertain",
        "cannot confirm the original tmux session has ended",
    )
}

fn boot_id() -> std::io::Result<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
}

fn process_start(pid: u32) -> Result<Option<u64>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    let mut fields = stat
        .rsplit_once(") ")
        .context("invalid process stat")?
        .1
        .split_whitespace();
    let state = fields.next().context("missing process state")?;
    if matches!(state, "Z" | "X") {
        return Ok(None);
    }
    Ok(Some(
        fields
            .nth(18)
            .context("missing process start time")?
            .parse()?,
    ))
}

#[derive(PartialEq)]
pub(super) enum Worker {
    LegacyPane,
    Started(u32),
    Exited,
}

impl Worker {
    pub(super) fn check_before_kill(&self) -> Reply<()> {
        if let Self::Started(pid) = self {
            if process_start(*pid).map_err(|_| unknown())?.is_none() {
                return Err(failure(
                    "uncertain",
                    "worker is gone but its exit receipt is pending",
                ));
            }
        }
        Ok(())
    }
}

impl ServerSession {
    fn capture(pid: u32, session: String, pane: Option<String>, socket: PathBuf) -> Result<Self> {
        Ok(Self {
            socket,
            launch_id: None,
            boot: boot_id()?,
            namespace: fs::metadata("/proc/self/ns/pid")?.ino(),
            pid,
            start_ticks: process_start(pid)?.context("tmux server has exited")?,
            session,
            pane,
        })
    }

    fn verify(&self, t: &Target) -> Reply<()> {
        if self.session == t.session
            && self.pid.to_string() == t.server_pid
            && self.server_alive().map_err(|_| unknown())?
        {
            Ok(())
        } else {
            Err(unknown())
        }
    }

    fn same_server(&self, other: &Self) -> bool {
        self.socket == other.socket
            && self.boot == other.boot
            && self.namespace == other.namespace
            && self.pid == other.pid
            && self.start_ticks == other.start_ticks
    }

    pub(super) fn server_alive(&self) -> Result<bool> {
        if self.boot != boot_id()? {
            return Ok(false);
        }
        ensure!(
            self.namespace == fs::metadata("/proc/self/ns/pid")?.ino(),
            "different PID namespace"
        );
        Ok(process_start(self.pid)? == Some(self.start_ticks))
    }
}

// Pin the filesystem socket while asking tmux for its session list. The
// kernel reports the peer PID in our namespace even if a replacement
// server reports the same small PID in a different namespace.
pub(super) fn pin_socket(path: &Path, pid: u32) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)?;
    let connection = UnixStream::connect(fd_path(&file))?;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    // Both output buffers live until getsockopt returns.
    let rc = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    ensure!(
        rc == 0 && u32::try_from(cred.pid).ok() == Some(pid),
        "tmux socket peer changed"
    );
    Ok(file)
}

pub(super) fn fd_path(file: &File) -> String {
    format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd())
}

pub(super) fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(Uuid::new_v4().simple().to_string());
    let f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&f, value)?;
        f.sync_all()?;
        Ok(fs::rename(&tmp, path)?)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(super) fn host_session() -> Result<ServerSession> {
    // TMUX ends in ",<server pid>,<numeric session id>". Socket names may contain commas.
    let tmux = env::var("TMUX")?;
    let mut parts = tmux.rsplitn(3, ',');
    let session: u32 = parts.next().context("missing tmux session")?.parse()?;
    let pid = parts.next().context("missing tmux PID")?.parse()?;
    let socket = parts.next().context("missing tmux socket")?;
    let pane = env::var("TMUX_PANE")?;
    ensure!(
        pane.strip_prefix('%')
            .is_some_and(|s| s.parse::<u32>().is_ok()),
        "invalid tmux pane ID"
    );
    ServerSession::capture(pid, format!("${session}"), Some(pane), socket.into())
}

pub(super) fn record_server(dir: &Path, id: &str) -> Result<()> {
    let mut binding = host_session()?;
    binding.launch_id = Some(id.to_owned());
    save(&dir.join(format!("{id}.server")), &binding)
}

pub(super) fn host_matches_created_session(path: &Path, id: &str, host: &ServerSession) -> bool {
    super::launch::read_receipt(path)
        .and_then(|value| serde_json::from_value::<ServerSession>(value).ok())
        .is_some_and(|record| {
            record.launch_id.as_deref() == Some(id)
                && record.same_server(host)
                && record.session == host.session
                && record.pane == host.pane
        })
}

// tmux calls this in its new-session command queue, independently of
// both the launch client receiving its response and the worker consuming the spec.
pub(crate) fn record_created_session(args: &[std::ffi::OsString]) -> Result<()> {
    let state = Path::new(&args[0]);
    let id = args[1].to_str().context("missing launch ID")?;
    let pid = args[2].to_str().context("missing server PID")?.parse()?;
    let session = args[3].to_str().context("missing session ID")?;
    let pane = args[4].to_str().context("missing pane ID")?;
    let socket = PathBuf::from(&args[5]);
    ensure!(
        valid_launch_id(id)
            && valid_session(session)
            && valid_pane(pane)
            && socket.is_absolute()
            && args[6] == "1"
            && args[7] == id,
        "invalid tmux creation identity"
    );
    let prefix = state.join("launches").join(id);
    let value = super::launch::read_receipt(&prefix.with_extension("submission"))
        .context("missing launch submission")?;
    let submission: super::launch::Submission = serde_json::from_value(value)?;
    ensure!(
        submission.launch_id == id
            && submission.phase == super::launch::SubmissionPhase::InFlight
            && submission
                .socket
                .as_ref()
                .is_none_or(|expected| *expected == socket),
        "unverified tmux creation"
    );
    pin_socket(&socket, pid)?;
    let mut binding =
        ServerSession::capture(pid, session.to_owned(), Some(pane.to_owned()), socket)?;
    ensure!(
        submission
            .origin
            .as_ref()
            .is_none_or(|origin| origin.same_server(&binding)),
        "tmux creation selected another server"
    );
    binding.launch_id = Some(id.to_owned());
    save(&prefix.with_extension("session"), &binding)
}

fn valid_session(id: &str) -> bool {
    id.strip_prefix('$')
        .is_some_and(|id| id.parse::<u32>().is_ok())
}

impl Tmux<'_> {
    pub(super) fn claim_path(&self) -> PathBuf {
        self.state
            .join("tmux")
            .join(format!("{}.json", self.dispatch))
    }

    pub(super) fn lock_dispatch(&self) -> Reply<File> {
        let dir = self.state.join("tmux");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|_| unknown())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.claim_path().with_extension("lock"))
            .map_err(|_| unknown())?;
        lock.lock().map_err(|_| unknown())?;
        Ok(lock)
    }

    pub(super) fn claim(&self) -> Reply<Option<Claim>> {
        match File::open(self.claim_path()) {
            Ok(f) => serde_json::from_reader(f).map(Some).map_err(|_| unknown()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(_) => Err(unknown()),
        }
    }

    pub(super) fn clear_anonymous_preflight(&self) -> Reply<()> {
        if matches!(self.claim()?, Some(Claim::LegacyPending {execution:None, socket, hints}) if socket == self.socket && hints == LegacyHints::default())
        {
            fs::remove_file(self.claim_path()).map_err(|_| unknown())?;
        }
        Ok(())
    }

    // An unlabelled session observation may precede the controller's first
    // identified request. Associate them only on the still-verified server.
    fn identify_observed(&self, stored: &mut Claim, id: &str) -> Reply<()> {
        let Claim::Observed {
            execution: None,
            hints,
            receipt,
            ..
        } = stored
        else {
            return Err(unknown());
        };
        let target = self.owned()?;
        let mut lookup = hints.lookup(self);
        let live = self.find_supervisor(&target, Some(id), &mut lookup)?;
        if let Some((launch, pid)) = live {
            if let Some(PaneEvidence::Bound(original)) = &hints.evidence {
                if hints.pane != lookup.pane || pid != original.pid || !original.live() {
                    return Err(unknown());
                }
            }
            if receipt.as_deref().is_some_and(|old| old != launch) {
                return Err(unknown());
            }
            if hints.pane.is_some()
                && hints.evidence.is_none()
                && receipt.is_none()
                && lookup.expected_launch(Some(id)).is_none()
            {
                return Err(unknown());
            }
            *hints = lookup;
            *receipt = Some(launch);
        } else {
            hints.merge(self)?;
            // The caller must supply the authoritative pane, even if this old
            // record contains a pane guessed from the active window.
            let pane = self.legacy_worker_pane.ok_or_else(unknown)?;
            if hints.launch.is_some() || valid_launch_id(id) {
                return Err(unknown());
            }
            self.guarded(
                &target,
                pane,
                "display-message -p verified",
                super::EXCLUSIVE_WINDOW,
            )?;
        }
        if let Claim::Observed { execution, .. } = stored {
            *execution = Some(id.to_owned());
        }
        save(&self.claim_path(), stored).map_err(|_| unknown())
    }

    pub(super) fn remember_existing(&self) -> Reply<()> {
        let Some(id) = self.legacy_execution_id else {
            return Ok(());
        };
        let claim = self.claim()?;
        let mut pending = LegacyHints::default();
        match claim {
            Some(Claim::LegacyPending {
                socket,
                execution,
                hints,
            }) => {
                if socket != self.socket || execution.as_deref().is_some_and(|old| old != id) {
                    return Err(unknown());
                }
                pending = hints;
            }
            Some(mut observed @ Claim::Observed { .. }) if observed.execution_id() == Some(id) => {
                if let Claim::Observed {
                    ref mut hints,
                    ref receipt,
                    ref server,
                    ..
                } = observed
                {
                    if self
                        .socket
                        .as_ref()
                        .is_some_and(|socket| *socket != server.socket)
                    {
                        return Err(unknown());
                    }
                    if self
                        .legacy_launch_id
                        .zip(receipt.as_deref())
                        .is_some_and(|(a, b)| a != b)
                    {
                        return Err(unknown());
                    }
                    // A first compatible pane hint must survive a disconnected
                    // observation. Resolve conflicting unverified hints on the live target.
                    let mut merged = hints.clone();
                    match merged.merge(self) {
                        Ok(()) if merged != *hints => {
                            *hints = merged;
                            save(&self.claim_path(), &observed).map_err(|_| unknown())?;
                        }
                        Err(e) if receipt.is_some() => return Err(e),
                        _ => (),
                    }
                }
                return Ok(());
            }
            Some(ref claim) if claim.execution_id() == Some(id) => return Ok(()),
            None | Some(Claim::Released(_)) => (),
            Some(
                mut previous @ Claim::Observed {
                    execution: None, ..
                },
            ) if self.legacy_worker_pane.is_some()
                || self.legacy_launch_id.is_some()
                || valid_launch_id(id) =>
            {
                if self.confirm_absence(Some(previous.clone()), true).is_err() {
                    return self.identify_observed(&mut previous, id);
                }
            }
            Some(previous) => self.confirm_absence(Some(previous), true)?,
        }
        if self.legacy_launch_id.is_some() {
            pending.launch = None;
        }
        if self.legacy_worker_pane.is_some() && self.legacy_worker_pane != pending.pane.as_deref() {
            pending.pane = None;
            pending.evidence = None;
        }
        pending.merge(self)?;
        self.replace_claim(&Claim::LegacyPending {
            execution: Some(id.to_owned()),
            socket: self.socket.clone(),
            hints: pending,
        })
    }

    pub(super) fn release(&self) -> Reply<()> {
        let claim = self.claim()?;
        let execution = claim
            .as_ref()
            .and_then(Claim::execution_id)
            .or(self.legacy_execution_id);
        self.replace_claim(&Claim::Released(execution.map(str::to_owned)))
    }

    pub(super) fn confirm_launch(&self, id: &str) -> Reply<Worker> {
        let server = self.binding(Claim::Launch(id.to_owned()))?;
        let path = self.state.join("launches").join(format!("{id}.json"));
        let receipt = super::launch::read_receipt(&path).ok_or_else(unknown)?;
        if server.launch_id.is_some() && receipt["launchId"] != id {
            return Err(unknown());
        }
        Self::worker_from_receipt(&receipt)
    }

    pub(super) fn close_worker(&self, t: &Target) -> Reply<Worker> {
        let id = match self.claim()? {
            Some(Claim::Observed { token, receipt, .. }) if token == t.token => match receipt {
                Some(id) if valid_launch_id(&id) => id,
                None => return Ok(Worker::LegacyPane),
                _ => return Err(unknown()),
            },
            Some(Claim::Launch(id) | Claim::Registered { id, .. }) if id == t.token => {
                return self.confirm_launch(&id)
            }
            _ => return Err(unknown()),
        };
        self.receipt_worker(&id)
    }

    pub(super) fn receipt_worker(&self, id: &str) -> Reply<Worker> {
        let path = self.state.join("launches").join(format!("{id}.json"));
        let receipt = super::launch::read_receipt(&path).unwrap_or_default();
        Self::worker_from_receipt(&receipt)
    }

    fn worker_from_receipt(receipt: &serde_json::Value) -> Reply<Worker> {
        match (receipt["category"].as_str(), receipt["exitCode"].as_u64()) {
            (Some("started"), _) => receipt["pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .map(Worker::Started)
                .ok_or_else(unknown),
            (Some("process_exited"), Some(0..=255)) => Ok(Worker::Exited),
            _ => Err(failure(
                "uncertain",
                "could not confirm the original worker's launch receipt",
            )),
        }
    }

    pub(super) fn worker_pane_absent(&self, t: &Target) -> Reply<bool> {
        let list = self.run(&[
            "-S",
            &fd_path(&t.socket_fd),
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}",
        ])?;
        let panes = std::str::from_utf8(&list.stdout).map_err(|_| unknown())?;
        if !list.status.success() || panes.is_empty() || !panes.lines().all(valid_pane) {
            return Err(unknown());
        }
        Ok(!panes.lines().any(|p| Some(p) == t.worker_pane.as_deref()))
    }

    fn binding(&self, claim: Claim) -> Reply<ServerSession> {
        match claim {
            Claim::LegacyPending { .. } | Claim::Released(_) => Err(unknown()),
            Claim::Observed {
                mut server, hints, ..
            } => {
                // Older Observed records sometimes contain a guessed pane.
                server.pane = hints.pane;
                Ok(server)
            }
            Claim::Launch(id) | Claim::Registered { id, .. } => {
                let id = Uuid::parse_str(&id).map_err(|_| unknown())?.simple();
                let path = self.state.join("launches").join(format!("{id}.server"));
                serde_json::from_value(super::launch::read_receipt(&path).ok_or_else(unknown)?)
                    .map_err(|_| unknown())
            }
        }
    }

    pub(super) fn has_original_server_record(&self, id: &str) -> bool {
        self.binding(Claim::Launch(id.to_owned()))
            .is_ok_and(|server| {
                valid_session(&server.session)
                    && server.pane.as_deref().is_some_and(valid_pane)
                    && server.socket.is_absolute()
                    && self
                        .socket
                        .as_ref()
                        .is_none_or(|socket| *socket == server.socket)
                    && server.pid != 0
                    && server.start_ticks != 0
                    && !server.boot.trim().is_empty()
            })
    }

    pub(super) fn launch_server(&self) -> Option<ServerSession> {
        let out = self
            .run(&["-N", "display-message", "-p", "#{pid}\n#{socket_path}"])
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let (pid, socket) = std::str::from_utf8(&out.stdout)
            .ok()?
            .strip_suffix('\n')?
            .split_once('\n')?;
        let pid = pid.parse().ok()?;
        let socket = self.socket.clone().unwrap_or_else(|| PathBuf::from(socket));
        pin_socket(&socket, pid).ok()?;
        ServerSession::capture(pid, String::new(), None, socket).ok()
    }

    pub(super) fn pin_submission(&self, origin: &ServerSession) -> Reply<File> {
        if !origin.server_alive().map_err(|_| unknown())? {
            return Err(unknown());
        }
        pin_socket(&origin.socket, origin.pid).map_err(|_| unknown())
    }

    fn created_session(&self, id: &str) -> Reply<Option<ServerSession>> {
        let path = self.launch_path(id, "session");
        if super::launch::missing(&path) {
            return Ok(None);
        }
        let value = super::launch::read_receipt(&path).ok_or_else(unknown)?;
        let server: ServerSession = serde_json::from_value(value).map_err(|_| unknown())?;
        if server.launch_id.as_deref() != Some(id)
            || !valid_session(&server.session)
            || !server.pane.as_deref().is_some_and(valid_pane)
        {
            return Err(unknown());
        }
        Ok(Some(server))
    }

    fn verify_cancelled_target(&self, id: &str, execution: Option<&str>, t: &Target) -> Reply<()> {
        let submission = self.submission(id, execution)?;
        if !t.owned || t.token != id || submission.phase == super::launch::SubmissionPhase::Prepared
        {
            return Err(unknown());
        }
        if let Some(origin) = submission.origin {
            if origin.pid.to_string() != t.server_pid
                || origin.socket != Path::new(&t.socket)
                || !origin.server_alive().map_err(|_| unknown())?
            {
                return Err(unknown());
            }
        }
        let binding = self.created_session(id)?.ok_or_else(unknown)?;
        binding.verify(t)?;
        if binding.pane.as_deref() != Some(&t.pane) || binding.socket != Path::new(&t.socket) {
            return Err(unknown());
        }
        // Check the session-local launch flag and the original startup command.
        // A retained dead pane keeps that command, but a replacement usually has another.
        let shown = self.run(&[
            "-S",
            &fd_path(&t.socket_fd),
            "show-environment",
            "-t",
            &t.session,
            "WFC_TMUX_LAUNCH",
        ])?;
        let start = format!("#{{m:*--tmux-host {id},#{{pane_start_command}}}}");
        if !shown.status.success()
            || shown.stdout != format!("WFC_TMUX_LAUNCH={id}\n").as_bytes()
            || !self
                .guarded(
                    t,
                    &t.pane,
                    "display-message -p verified",
                    &format!("#{{&&:{EXCLUSIVE_WINDOW},{start}}}"),
                )
                .is_ok_and(|answer| answer == "verified\n")
        {
            return Err(unknown());
        }
        Ok(())
    }

    pub(super) fn close_cancelled_session(&self, id: &str, t: &Target) -> Reply<()> {
        // If the reply is lost, the presence of this marker prevents
        // retrying the same ambiguous action as a confirmed already-absent.
        let pending = self.launch_path(id, "cleanup");
        let start = format!("#{{m:*--tmux-host {id},#{{pane_start_command}}}}");
        // An unrelated pane or window may have been attached since new-session.
        let condition = format!("#{{&&:{start},#{{&&:{EXCLUSIVE_WINDOW},#{{&&:#{{pane_dead}},#{{&&:#{{==:#{{session_windows}},1}},#{{==:#{{window_panes}},1}}}}}}}}}}");
        let action = format!("kill-session -t '{}'", t.session);
        save(&pending, &self.close_operation_id).map_err(|_| unknown())?;
        match self.guarded(t, &t.pane, &action, &condition) {
            Err(error)
                if error["result"] == "refused" || error["result"] == "definitely-not-sent" =>
            {
                fs::remove_file(pending).map_err(|_| unknown())?;
                Err(error)
            }
            outcome => outcome.map(|_| ()),
        }
    }

    pub(super) fn confirm_cancelled_absence(
        &self,
        id: &str,
        execution: Option<&str>,
        reachable: bool,
    ) -> Reply<()> {
        use super::launch::SubmissionPhase;
        let submission = self.submission(id, execution)?;
        let binding = self.created_session(id)?;
        if submission.phase == SubmissionPhase::Prepared {
            return if binding.is_none() {
                Ok(())
            } else {
                Err(unknown())
            };
        }
        let server = match &binding {
            Some(server) => {
                if submission
                    .origin
                    .as_ref()
                    .is_some_and(|origin| !origin.same_server(server))
                    || self
                        .socket
                        .as_ref()
                        .is_some_and(|socket| *socket != server.socket)
                {
                    return Err(unknown());
                }
                server
            }
            None => {
                let origin = submission.origin.as_ref().ok_or_else(unknown)?;
                return if origin.server_alive().map_err(|_| unknown())? {
                    Err(unknown())
                } else {
                    Ok(())
                };
            }
        };
        if !server.server_alive().map_err(|_| unknown())? {
            return Ok(());
        }
        if !reachable {
            return Err(unknown());
        }
        let socket = pin_socket(&server.socket, server.pid).map_err(|_| unknown())?;
        let format = "#{session_id} #{pane_id} #{session_name} :#{WFC_TMUX_LAUNCH}";
        let list = self.run(&["-S", &fd_path(&socket), "list-panes", "-a", "-F", format])?;
        let listing = std::str::from_utf8(&list.stdout).map_err(|_| unknown())?;
        if list.status.success()
            && !listing.is_empty()
            && listing.lines().all(|line| {
                let mut fields = line.splitn(3, ' ');
                let (Some(session), Some(pane), Some(rest)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    return false;
                };
                let Some((name, launch)) = rest.rsplit_once(" :") else {
                    return false;
                };
                valid_session(session)
                    && valid_pane(pane)
                    && name != self.session
                    && launch != id
                    && binding.as_ref().is_none_or(|saved| {
                        session != saved.session && Some(pane) != saved.pane.as_deref()
                    })
            })
        {
            Ok(())
        } else {
            Err(unknown())
        }
    }

    pub(super) fn confirm_absence(&self, claim: Option<Claim>, reachable: bool) -> Reply<()> {
        let claim = match claim {
            None if self.protect_offline && !reachable => {
                return Err(unknown());
            }
            Some(Claim::Released(Some(id))) => {
                let execution = self.archived_execution(&id)?;
                if self
                    .unverified_released_cancellation(&id, execution.as_deref())?
                    .is_some()
                {
                    return Err(unknown());
                }
                return Ok(());
            }
            None | Some(Claim::Released(None)) => return Ok(()),
            Some(Claim::Launch(ref id) | Claim::Registered { ref id, .. }) => {
                let execution = match &claim {
                    Some(Claim::Registered { execution, .. }) => Some(execution.as_str()),
                    _ => None,
                };
                if self.cancellation(id, execution)?.is_some() {
                    return self.confirm_cancelled_absence(id, execution, reachable);
                }
                claim.expect("matched launch claim")
            }
            Some(claim) => claim,
        };
        let binding = self.binding(claim)?;
        if !binding.server_alive().map_err(|_| unknown())? {
            return Ok(());
        }
        if reachable && binding.pane.as_deref().is_some_and(valid_pane) {
            // The name disappeared. Check the exact incarnation and ID as well,
            // since a session can be renamed or the socket can select a different server.
            let socket = pin_socket(&binding.socket, binding.pid).map_err(|_| unknown())?;
            let list = self
                .run(&[
                    "-S",
                    &fd_path(&socket),
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_id} #{pane_id}",
                ])
                .map_err(|_| unknown())?;
            let listing = String::from_utf8_lossy(&list.stdout);
            if list.status.success()
                && !listing.is_empty()
                && listing.lines().all(|l| {
                    l.split_once(' ').is_some_and(|(session, pane)| {
                        session.starts_with('$')
                            && pane.starts_with('%')
                            && session != binding.session
                            && Some(pane) != binding.pane.as_deref()
                    })
                })
            {
                return Ok(());
            }
        }
        Err(unknown())
    }

    fn pane_process(&self, target: &Target, pane: &str) -> Option<PaneProcess> {
        let output = self
            .guarded(
                target,
                pane,
                &format!("display-message -p -t {pane} '#{{pane_pid}}'"),
                "#{==:#{pane_dead},0}",
            )
            .ok()?;
        let pid = output.trim().parse().ok()?;
        Some(PaneProcess {
            pid,
            ticks: process_start(pid).ok()??,
        })
    }

    // The launcher writes the launch ID in the supervisor argv and the actual worker PID
    // in its private receipt. Require both on the same pane before retaining
    // that receipt for a later close. The filename alone proves no association.
    fn find_supervisor(
        &self,
        t: &Target,
        execution: Option<&str>,
        hints: &mut LegacyHints,
    ) -> Reply<Option<(String, u32)>> {
        let expected = hints.expected_launch(execution);
        if hints.pane.is_none() && expected.is_none() {
            return Ok(None);
        }
        let out = self.run(&[
            "-S",
            &fd_path(&t.socket_fd),
            "list-panes",
            "-s",
            "-t",
            &t.session,
            "-F",
            "#{pane_id} #{pane_pid}",
        ])?;
        if !out.status.success() {
            return Err(unknown());
        }
        for line in std::str::from_utf8(&out.stdout)
            .map_err(|_| unknown())?
            .lines()
        {
            let (pane, pid) = line.split_once(' ').ok_or_else(unknown)?;
            if !valid_pane(pane) || hints.pane.as_deref().is_some_and(|hint| hint != pane) {
                continue;
            }
            let pid: u32 = pid.parse().map_err(|_| unknown())?;
            let Some(id) = host_launch_id(pid, self.state, self.dispatch, t) else {
                continue;
            };
            if expected.is_some_and(|wanted| wanted != id) {
                continue;
            }
            hints.pane = Some(pane.to_owned());
            return Ok(Some((id, pid)));
        }
        Ok(None)
    }

    pub(super) fn verify_claim(&self, claim: Option<Claim>, t: &mut Target) -> Reply<()> {
        let (execution, mut hints, receipt, server, adopting) = match claim {
            Some(claim @ (Claim::Launch(_) | Claim::Registered { .. })) => {
                let (id, execution) = match claim {
                    Claim::Launch(id) => (id, None),
                    Claim::Registered { id, execution } => (id, Some(execution)),
                    _ => unreachable!(),
                };
                if t.token != id {
                    return Err(unknown());
                }
                if self.cancellation(&id, execution.as_deref())?.is_some() {
                    return self.verify_cancelled_target(&id, execution.as_deref(), t);
                }
                let server = self.binding(Claim::Launch(id.clone()))?;
                server.verify(t)?;
                if !server.pane.as_deref().is_some_and(valid_pane) {
                    return Err(unknown());
                }
                let mut hints = LegacyHints {
                    pane: server.pane.clone(),
                    launch: Some(id),
                    evidence: None,
                };

                if t.owned {
                    t.supervisor = self.find_supervisor(t, None, &mut hints)?;
                }
                t.worker_pane = server.pane;
                return Ok(());
            }
            Some(Claim::Observed {
                execution,
                hints,
                receipt,
                token,
                server,
            }) => {
                if t.token != token
                    || self
                        .socket
                        .as_ref()
                        .is_some_and(|socket| *socket != server.socket)
                {
                    return Err(unknown());
                }
                server.verify(t)?;
                (execution, hints, receipt, server, false)
            }
            Some(Claim::LegacyPending { socket, .. }) if socket != self.socket || !t.owned => {
                return Err(unknown())
            }
            Some(Claim::Released(Some(closed)))
                if t.token == closed
                    && self
                        .unverified_released_cancellation(
                            &closed,
                            self.archived_execution(&closed)?.as_deref(),
                        )?
                        .is_some() =>
            {
                return Err(unknown())
            }
            Some(Claim::Released(Some(closed)))
                if Some(closed.as_str()) == self.legacy_execution_id =>
            {
                return Err(unknown());
            }
            _ if !t.owned => return Ok(()),
            other => {
                let (execution, hints) = match other {
                    Some(Claim::LegacyPending {
                        execution, hints, ..
                    }) => (execution, hints),
                    _ => (
                        self.legacy_execution_id.map(str::to_owned),
                        LegacyHints::default(),
                    ),
                };
                let server = ServerSession::capture(
                    t.server_pid.parse().map_err(|_| unknown())?,
                    t.session.clone(),
                    None,
                    t.socket.clone().into(),
                )
                .map_err(|_| unknown())?;
                (execution, hints, None, server, true)
            }
        };
        let saved = hints.clone();
        if adopting {
            if let Some(evidence) = &saved.evidence {
                evidence.verify_server(&server)?;
            }
        }
        if receipt.is_none() && self.legacy_execution_id.is_some() {
            hints = saved.lookup(self);
        }
        let checked = if t.owned {
            self.find_supervisor(t, execution.as_deref(), &mut hints)?
        } else {
            None
        };
        // A stored pane without process evidence (including pre-R7 rows)
        // cannot attest to whoever happens to occupy that pane today.
        let original = match (&saved.pane, &saved.evidence) {
            (Some(pane), Some(evidence)) if receipt.is_none() => {
                self.pane_process(t, pane).filter(|p| evidence.matches(p))
            }
            _ => None,
        };
        let pinned = matches!(saved.evidence, Some(PaneEvidence::Bound(_)));
        let pane_only =
            saved.pane.is_some() && hints.expected_launch(execution.as_deref()).is_none();
        let checked = checked.filter(|(_, pid)| {
            receipt.is_some()
                || (!pinned && !pane_only)
                || (saved.pane == hints.pane && original.as_ref().is_some_and(|p| p.pid == *pid))
        });
        if checked.is_none() && saved != hints {
            let mut compatible = saved.clone();
            compatible.merge(self)?;
            hints = compatible;
        }
        if let Some(process) = original.filter(|_| saved.pane == hints.pane) {
            if hints.expected_launch(execution.as_deref()).is_none() {
                hints.evidence = Some(PaneEvidence::Bound(process.clone()));
                t.pane_process = Some(process);
            }
        }
        t.supervisor = checked.filter(|(id, _)| receipt.as_ref().is_none_or(|saved| saved == id));
        let checked = t.supervisor.as_ref().map(|(id, _)| id.clone());
        if self.input_request && receipt.is_some() && checked != receipt {
            return Err(failure(
                "uncertain",
                "original legacy worker is no longer confirmed in its pane",
            ));
        }
        let newly_verified = receipt.is_none() && checked.is_some();

        t.worker_pane = if hints.expected_launch(execution.as_deref()).is_some()
            && checked.is_none()
            && receipt.is_none()
        {
            None
        } else {
            hints.pane.clone()
        };
        if adopting {
            let token = Uuid::new_v4().simple().to_string();
            self.guarded(
                t,
                &t.pane,
                &format!("set-environment -t '{}' WFC_TMUX_LAUNCH {token}", t.session),
                super::EXCLUSIVE_WINDOW,
            )?;
            t.token = token;
        }
        if adopting || newly_verified || saved != hints {
            self.replace_claim(&Claim::Observed {
                execution,
                hints,
                receipt: receipt.or(checked),
                server,
                token: t.token.clone(),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::listener_inode;
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};

    #[test]
    fn listening_address_keeps_path_bytes_and_accepts_padded_inodes() {
        let path = Path::new(OsStr::from_bytes(b"/tmp/x \xff"));
        assert_eq!(
            listener_inode(b"0000: 0003 0000 00010000 0001 01    42 /tmp/x \xff", path),
            Some(42)
        );
        assert_eq!(
            listener_inode(b"0000: 0003 0000 00010000 0001 01    42 @/tmp/x \xff", path),
            None
        );
    }
}
