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
    Launch(String),
    Observed {
        server: ServerSession,
        token: String,
    },
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

    pub(super) fn forget(&self) -> Reply<()> {
        match fs::remove_file(self.claim_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(unknown()),
        }
    }

    fn binding(&self, claim: Claim) -> Reply<ServerSession> {
        match claim {
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
        let Some(claim) = claim else {
            return Ok(());
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
            Some(claim) => {
                let token = match &claim {
                    Claim::Launch(id) => id,
                    Claim::Observed { token, .. } => token,
                };
                if t.token != *token {
                    return Err(unknown());
                }
                let server = self.binding(claim)?;
                if t.session != server.session
                    || t.server_pid != server.pid.to_string()
                    || !server.server_alive().map_err(|_| unknown())?
                {
                    return Err(unknown());
                }
                if !server
                    .pane
                    .strip_prefix('%')
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                {
                    return Err(unknown());
                }
                t.worker_pane = server.pane;
                Ok(())
            }
            None => {
                // Adopt a reachable owned session from before claims existed.
                let server = ServerSession::capture(
                    t.server_pid.parse().map_err(|_| unknown())?,
                    t.session.clone(),
                    t.pane.clone(),
                    t.socket.clone().into(),
                )
                .map_err(|_| unknown())?;
                // Even a restored environment may contain a previous token.
                // Each fresh adoption invalidates attach commands from older claims.
                let token = Uuid::new_v4().simple().to_string();
                self.guarded(
                    t,
                    &format!("set-environment -t '{}' WFC_TMUX_LAUNCH {token}", t.session),
                    "1",
                )?;
                t.token = token;
                save(
                    &self.claim_path(),
                    &Claim::Observed {
                        server,
                        token: t.token.clone(),
                    },
                )
                .map_err(|_| unknown())
            }
        }
    }
}
