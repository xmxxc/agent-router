//! A missing socket is not evidence that an earlier tmux server stopped.
//! Reserve the dispatch before new-session. The host records the server it
//! actually runs under before spawning the worker, even if the client reply is lost.
use super::{failure, Reply, Target, Tmux};
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

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct LegacyHints {
    pane: Option<String>,
    launch: Option<String>,
}

impl LegacyHints {
    fn merge(&mut self, t: &Tmux<'_>) -> Reply<()> {
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

fn host_launch_id(pid: u32, state: &Path, dispatch: &str, target: &Target) -> Option<String> {
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
    let parent = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse::<u32>()
        .ok()?;
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

    fn server_alive(&self) -> Result<bool> {
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

pub(super) fn record_server(dir: &Path, id: &str) -> Result<()> {
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
    let binding = ServerSession::capture(pid, format!("${session}"), Some(pane), socket.into())?;
    save(&dir.join(format!("{id}.server")), &binding)
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
        hints.merge(self)?;
        let live = self.legacy_worker(&target, Some(id), hints)?;
        if let Some((launch, _)) = live {
            if receipt.as_deref().is_some_and(|old| old != launch) {
                return Err(unknown());
            }
            *receipt = Some(launch);
        } else {
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
                    let old = hints.clone();
                    hints.merge(self)?;
                    if old != *hints {
                        save(&self.claim_path(), &observed).map_err(|_| unknown())?;
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

    pub(super) fn confirm_launch(&self, id: &str) -> Reply<()> {
        self.binding(Claim::Launch(id.to_owned()))?;
        self.receipt_worker(id).map(|_| ())
    }

    pub(super) fn close_worker(&self, t: &Target) -> Reply<Worker> {
        let id = match self.claim()? {
            Some(Claim::Observed { token, receipt, .. }) if token == t.token => match receipt {
                Some(id) if valid_launch_id(&id) => id,
                None => return Ok(Worker::LegacyPane),
                _ => return Err(unknown()),
            },
            Some(Claim::Launch(id) | Claim::Registered { id, .. }) if id == t.token => id,
            _ => return Err(unknown()),
        };
        self.receipt_worker(&id)
    }

    pub(super) fn receipt_worker(&self, id: &str) -> Reply<Worker> {
        let path = self.state.join("launches").join(format!("{id}.json"));
        let receipt = super::launch::read_receipt(&path).unwrap_or_default();
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
                let f = File::open(self.state.join("launches").join(format!("{id}.server")))
                    .map_err(|_| unknown())?;
                serde_json::from_reader(f).map_err(|_| unknown())
            }
        }
    }

    pub(super) fn confirm_absence(&self, claim: Option<Claim>, reachable: bool) -> Reply<()> {
        let claim = match claim {
            None if self.protect_offline && !reachable => {
                return Err(unknown());
            }
            None | Some(Claim::Released(_)) => return Ok(()),
            Some(Claim::Launch(ref id) | Claim::Registered { ref id, .. })
                if super::launch::cancelled(self.state, id) =>
            {
                return Ok(())
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

    // R1 writes the launch ID in the supervisor argv and the actual worker PID
    // in its private receipt. Require both on the same pane before retaining
    // that receipt for a later close. The filename alone proves no association.
    fn legacy_worker(
        &self,
        t: &Target,
        execution: Option<&str>,
        hints: &mut LegacyHints,
    ) -> Reply<Option<(String, u32)>> {
        let expected = hints
            .launch
            .as_deref()
            .or_else(|| execution.filter(|id| valid_launch_id(id)));
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
                if hints.pane.is_some() {
                    return Err(unknown());
                }
                continue;
            }
            hints.pane = Some(pane.to_owned());
            return Ok(Some((id, pid)));
        }
        Ok(None)
    }

    pub(super) fn verify_claim(&self, claim: Option<Claim>, t: &mut Target) -> Reply<()> {
        let (execution, mut hints, receipt, server, adopting) = match claim {
            Some(Claim::Launch(id) | Claim::Registered { id, .. }) => {
                if t.token != id {
                    return Err(unknown());
                }
                let server = self.binding(Claim::Launch(id))?;
                server.verify(t)?;
                if !server.pane.as_deref().is_some_and(valid_pane) {
                    return Err(unknown());
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
                if Some(closed.as_str()) == self.legacy_execution_id =>
            {
                return Err(unknown())
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
        let checked = if t.owned && (receipt.is_none() || self.input_request) {
            self.legacy_worker(t, execution.as_deref(), &mut hints)?
        } else {
            None
        };
        let (checked, host_pid) = checked.unzip();
        t.legacy_host_pid = host_pid;
        if self.input_request && receipt.is_some() && checked != receipt {
            return Err(failure(
                "uncertain",
                "original legacy worker is no longer confirmed in its pane",
            ));
        }
        let newly_verified = receipt.is_none() && checked.is_some();
        t.worker_pane = if hints.launch.is_some() && checked.is_none() && receipt.is_none() {
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
        if adopting || newly_verified {
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
