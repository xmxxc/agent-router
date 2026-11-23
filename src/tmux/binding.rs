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

#[derive(Deserialize, Serialize)]
pub(super) enum Claim {
    LegacyPending {
        execution: Option<String>,
        socket: Option<PathBuf>,
        pane: Option<String>,
    },
    Released(Option<String>),
    Launch(String),
    Observed {
        execution: Option<String>,
        server: ServerSession,
        token: String,
    },
}

impl Claim {
    fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Launch(id) => Some(id),
            Self::Observed {
                execution, token, ..
            } => Some(execution.as_deref().unwrap_or(token)),
            Self::Released(id) | Self::LegacyPending { execution: id, .. } => id.as_deref(),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct ServerSession {
    socket: PathBuf,
    boot: String,
    namespace: u64,
    pid: u32,
    start_ticks: u64,
    session: String,
    pane: String,
}

pub(super) fn valid_pane(id: &str) -> bool {
    id.strip_prefix('%')
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn unknown() -> serde_json::Value {
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
    fn capture(pid: u32, session: String, pane: String, socket: PathBuf) -> Result<Self> {
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
    let binding = ServerSession::capture(pid, format!("${session}"), pane, socket.into())?;
    save(&dir.join(format!("{id}.server")), &binding)
}

impl Tmux<'_> {
    fn claim_path(&self) -> PathBuf {
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

    pub(super) fn reserve(&self, id: &str) -> Reply<()> {
        save(&self.claim_path(), &Claim::Launch(id.to_owned())).map_err(|_| unknown())
    }

    pub(super) fn remember_existing(&self) -> Reply<()> {
        let (execution, pane) = match self.claim()? {
            Some(Claim::LegacyPending {
                socket,
                execution,
                pane,
            }) => {
                if socket != self.socket
                    || execution
                        .as_deref()
                        .zip(self.legacy_execution_id)
                        .is_some_and(|(a, b)| a != b)
                    || pane
                        .as_deref()
                        .zip(self.legacy_worker_pane)
                        .is_some_and(|(a, b)| a != b)
                {
                    return Err(unknown());
                }
                if (execution.is_some() || self.legacy_execution_id.is_none())
                    && (pane.is_some() || self.legacy_worker_pane.is_none())
                {
                    return Ok(());
                }
                (execution, pane)
            }
            Some(ref claim) if claim.execution_id() == self.legacy_execution_id => return Ok(()),
            None | Some(Claim::Released(_)) => (None, None),
            Some(previous) => {
                // Its original server can prove the previous execution ended.
                // That proves nothing about the newly supplied legacy execution.
                self.confirm_absence(Some(previous), true)?;
                (None, None)
            }
        };
        save(
            &self.claim_path(),
            &Claim::LegacyPending {
                execution: execution.or_else(|| self.legacy_execution_id.map(str::to_owned)),
                socket: self.socket.clone(),
                pane: pane.or_else(|| self.legacy_worker_pane.map(str::to_owned)),
            },
        )
        .map_err(|_| unknown())
    }

    pub(super) fn release(&self) -> Reply<()> {
        let claim = self.claim()?;
        let execution = claim
            .as_ref()
            .and_then(Claim::execution_id)
            .or(self.legacy_execution_id);
        save(
            &self.claim_path(),
            &Claim::Released(execution.map(str::to_owned)),
        )
        .map_err(|_| unknown())
    }

    pub(super) fn close_worker(&self, t: &Target) -> Reply<Worker> {
        match self.claim()? {
            Some(Claim::Observed { token, .. }) if token == t.token => {
                return Ok(Worker::LegacyPane)
            }
            Some(Claim::Launch(id)) if id == t.token => (),
            _ => return Err(unknown()),
        }
        let path = self
            .state
            .join("launches")
            .join(format!("{}.json", t.token));
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
        Ok(!panes.lines().any(|p| p == t.worker_pane))
    }

    fn binding(&self, claim: Claim) -> Reply<ServerSession> {
        match claim {
            Claim::LegacyPending { .. } | Claim::Released(_) => Err(unknown()),
            Claim::Observed { server, .. } => Ok(server),
            Claim::Launch(id) => {
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
                self.remember_existing()?;
                return Err(unknown());
            }
            None | Some(Claim::Released(_)) => return Ok(()),
            Some(Claim::Launch(ref id)) if super::launch::cancelled(self.state, id) => {
                return Ok(())
            }
            Some(claim) => claim,
        };
        let binding = self.binding(claim)?;
        if !binding.server_alive().map_err(|_| unknown())? {
            return Ok(());
        }
        if reachable {
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
                            && pane != binding.pane
                    })
                })
            {
                return Ok(());
            }
        }
        Err(unknown())
    }

    pub(super) fn verify_claim(&self, claim: Option<Claim>, t: &mut Target) -> Reply<()> {
        match claim {
            Some(Claim::Launch(ref token) | Claim::Observed { ref token, .. })
                if *token != t.token =>
            {
                Err(unknown())
            }
            Some(claim @ (Claim::Launch(_) | Claim::Observed { .. })) => {
                let server = self.binding(claim)?;
                if t.session != server.session
                    || t.server_pid != server.pid.to_string()
                    || !server.server_alive().map_err(|_| unknown())?
                {
                    return Err(unknown());
                }
                if !valid_pane(&server.pane) {
                    return Err(unknown());
                }
                t.worker_pane = server.pane;
                Ok(())
            }
            Some(Claim::LegacyPending { ref socket, .. }) if socket != &self.socket || !t.owned => {
                Err(unknown())
            }
            Some(Claim::Released(Some(ref closed)))
                if Some(closed.as_str()) == self.legacy_execution_id =>
            {
                // A closed legacy execution cannot own a newly visible session.
                Err(unknown())
            }
            _ if !t.owned => Ok(()),
            unbound => {
                let (hint, execution) = match unbound {
                    Some(Claim::LegacyPending {
                        pane, execution, ..
                    }) => (pane, execution),
                    _ => (None, self.legacy_execution_id.map(str::to_owned)),
                };
                // Adopt a reachable owned session from before claims existed.
                let panes = self.run(&[
                    "-S",
                    &fd_path(&t.socket_fd),
                    "list-panes",
                    "-s",
                    "-t",
                    &t.session,
                    "-F",
                    "#{pane_id}",
                ])?;
                let listing = std::str::from_utf8(&panes.stdout).map_err(|_| unknown())?;
                let pane = match hint.as_deref() {
                    Some(id) if listing.lines().any(|p| p == id) => id,
                    None if listing.lines().count() == 1 => listing.trim_end(),
                    _ => {
                        return Err(failure(
                            "uncertain",
                            "could not identify the legacy worker pane",
                        ))
                    }
                };
                if !panes.status.success() || !valid_pane(pane) {
                    return Err(unknown());
                }
                let server = ServerSession::capture(
                    t.server_pid.parse().map_err(|_| unknown())?,
                    t.session.clone(),
                    pane.to_owned(),
                    t.socket.clone().into(),
                )
                .map_err(|_| unknown())?;
                // Even a restored environment may contain a previous token.
                // Each fresh adoption invalidates attach commands from older claims.
                let token = Uuid::new_v4().simple().to_string();
                self.guarded(
                    t,
                    pane,
                    &format!("set-environment -t '{}' WFC_TMUX_LAUNCH {token}", t.session),
                    super::EXCLUSIVE_WINDOW,
                )?;
                t.worker_pane = pane.to_owned();
                t.token = token;
                save(
                    &self.claim_path(),
                    &Claim::Observed {
                        execution,
                        server,
                        token: t.token.clone(),
                    },
                )
                .map_err(|_| unknown())
            }
        }
    }
}
