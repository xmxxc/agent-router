//! Linux tmux execution seam. The workflow owns attempts, serialization of lifecycle
//! operations, and delivery receipts. Each call can run in a fresh CLI process.
mod binding;
mod input;
pub(crate) mod launch;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Deserialize)]
pub struct Request {
    dispatch: String,
    backend: Option<String>,
    #[serde(flatten)]
    op: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Operation {
    Create(launch::Launch),
    Inspect,
    Read,
    Submit {
        text: String,
        host: Option<String>,
    },
    Interrupt,
    Resume {
        #[serde(flatten)]
        launch: launch::Launch,
        host: Option<String>,
        text: Option<String>,
        #[serde(default)]
        probe_read_only: bool,
    },
    Close,
}

type Reply<T> = std::result::Result<T, Value>;

fn failure(result: &str, diagnostic: &str) -> Value {
    json!({"result":result, "diagnostic":diagnostic})
}

struct Tmux<'a> {
    state: &'a Path,
    dispatch: &'a str,
    session: String,
    socket: Option<PathBuf>,
}

const EXCLUSIVE_WINDOW: &str = "#{==:#{window_linked_sessions},1}";

struct Target {
    socket_fd: File,
    worker_pane: String,
    token: String,
    socket: String,
    server_start: String,
    session: String,
    pane: String,
    server_pid: String,
    owner: String,
    owned: bool,
}

fn tmux_arg(value: &str) -> String {
    // tmux's argv parser consumes a final semicolon even after the shell is gone.
    match value.strip_suffix(';') {
        Some(prefix) => format!("{prefix}\\;"),
        None => value.to_owned(),
    }
}

impl Tmux<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        if let Some(socket) = &self.socket {
            command.arg("-S").arg(socket);
        }
        command
            .arg("-u")
            .env("LC_ALL", "C")
            .env_remove("WFC_STATE")
            .env_remove("WFC_DISPATCH")
            .env_remove("WFC_TMUX_LAUNCH");
        for (key, _) in env::vars_os() {
            if crate::host_launch::router_credential(&key.to_string_lossy()) {
                command.env_remove(key);
            }
        }
        command
    }

    fn run(&self, args: &[&str]) -> Reply<Output> {
        // Separate spawn from waiting: only a failed spawn proves tmux did nothing.
        let mut command = self.command();
        command.args(args.iter().map(|s| tmux_arg(s)));
        use std::process::Stdio;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| failure("definitely-not-sent", "tmux could not run"))?;
        child
            .wait_with_output()
            .map_err(|_| failure("uncertain", "tmux response unavailable"))
    }

    fn target(&self) -> Reply<Option<Target>> {
        let claim = self.claim()?;
        let selector = format!("={}:", self.session);
        let found = self.run(&[
            "list-panes",
            "-t",
            &selector,
            "-f",
            "#{pane_active}",
            "-F",
            "#{session_id} #{pane_id} #{pid} #{start_time} :#{WFC_TMUX_LAUNCH}\n#{socket_path}",
        ])?;
        if !found.status.success() {
            let err = String::from_utf8_lossy(&found.stderr);
            let missing_name = err.trim_end() == format!("can't find session: {}", self.session);
            if missing_name
                || err.starts_with("no server running on ")
                || (err.starts_with("error connecting to ")
                    && err.ends_with(" (No such file or directory)\n"))
            {
                self.confirm_absence(claim, missing_name)?;
                return Ok(None);
            }
            return Err(failure("uncertain", "tmux could not inspect the session"));
        }
        let ids = String::from_utf8_lossy(&found.stdout);
        let (ids, socket) = ids
            .strip_suffix('\n')
            .and_then(|s| s.split_once('\n'))
            .filter(|(_, socket)| Path::new(socket).is_absolute())
            .ok_or_else(|| failure("uncertain", "invalid tmux socket identity"))?;
        let fields: Vec<_> = ids.split_whitespace().collect();
        let [session, pane, server_pid, server_start, marked_token] = fields.as_slice() else {
            return Err(failure("uncertain", "invalid tmux target identity"));
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let token = marked_token
            .strip_prefix(':')
            .filter(|s| s.is_empty() || (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())))
            .ok_or_else(|| failure("uncertain", "invalid tmux launch identity"))?;
        if !digits(server_start)
            || !session.strip_prefix('$').is_some_and(digits)
            || !pane.strip_prefix('%').is_some_and(digits)
            || !digits(server_pid)
        {
            return Err(failure("uncertain", "invalid tmux target identity"));
        }
        let socket = self.socket.as_deref().unwrap_or(Path::new(socket));
        let socket_fd = binding::pin_socket(
            socket,
            server_pid
                .parse()
                .map_err(|_| failure("uncertain", "invalid tmux PID"))?,
        )
        .map_err(|_| failure("uncertain", "could not verify tmux socket peer"))?;
        let shown = self.run(&[
            "-S",
            &binding::fd_path(&socket_fd),
            "show-environment",
            "-t",
            session,
            "WFC_STATE",
        ])?;
        if !shown.status.success()
            && String::from_utf8_lossy(&shown.stderr).trim_end() != "unknown variable: WFC_STATE"
        {
            return Err(failure("uncertain", "could not confirm the session owner"));
        }
        let owner = std::str::from_utf8(&shown.stdout)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .and_then(|s| s.strip_prefix("WFC_STATE="))
            .unwrap_or("");
        let owned = shown.status.success()
            && Path::new(owner).is_absolute()
            && fs::canonicalize(owner).is_ok_and(|p| p == self.state);
        let mut t = Target {
            token: token.to_owned(),
            socket: socket.to_string_lossy().into_owned(),
            socket_fd,
            worker_pane: pane.to_string(),
            server_start: server_start.to_string(),
            session: session.to_string(),
            pane: pane.to_string(),
            server_pid: server_pid.to_string(),
            owner: owner.to_owned(),
            owned,
        };
        if claim.is_some() || owned {
            self.verify_claim(claim, &mut t)?;
        }
        Ok(Some(t))
    }

    fn attach_argv(&self, t: &Target) -> Reply<Vec<String>> {
        let binary = env::current_exe()
            .map_err(|_| failure("uncertain", "cannot locate tmux attach helper"))?;
        Ok(vec![
            binary.to_string_lossy().into_owned(),
            "--tmux-attach".into(),
            self.state.to_string_lossy().into_owned(),
            self.dispatch.into(),
            t.socket.clone(),
            t.token.clone(),
        ])
    }

    fn inspect(&self) -> Reply<Value> {
        Ok(match self.target()? {
            None => json!({"result":"accepted","alive":false,"owned":false}),
            Some(t) if t.owned => json!({"result":"accepted","alive":true,"owned":true,
                "session":self.session, "attach":self.attach_argv(&t)?}),
            Some(_) => json!({"result":"accepted","alive":true,"owned":false,
                "diagnostic":"session ownership does not match WFC_STATE"}),
        })
    }

    fn owned(&self) -> Reply<Target> {
        match self.target()? {
            Some(t) if t.owned => Ok(t),
            Some(_) => Err(failure(
                "refused",
                "session ownership does not match WFC_STATE",
            )),
            None => Err(failure("definitely-not-sent", "tmux session is absent")),
        }
    }

    // Check the live binding and execute in one server command queue. A stale
    // name must never send a later Enter or kill to a replacement session.
    fn guard(&self, t: &Target, condition: &str) -> String {
        let owner = t
            .owner
            .replace('#', "##")
            .replace(',', "#,")
            .replace('}', "#}");
        let checks = [
            ("pid", t.server_pid.as_str()),
            ("start_time", &t.server_start),
            ("session_id", &t.session),
            ("session_name", &self.session),
            ("pane_id", &t.pane),
            ("WFC_STATE", &owner),
            ("WFC_TMUX_LAUNCH", &t.token),
        ];
        checks
            .into_iter()
            .fold(condition.to_owned(), |rest, (field, value)| {
                format!("#{{&&:#{{==:#{{{field}}},{value}}},{rest}}}")
            })
    }

    fn guarded(&self, t: &Target, action: &str, condition: &str) -> Reply<String> {
        let guard = self.guard(t, condition);
        // A format uses the global environment as a fallback. Require the
        // session-local owner entry to still exist in the same command queue.
        let yes = format!(
            "show-environment -t '{}' WFC_STATE ; {action} ; display-message -p wfc-tmux-confirmed",
            t.session
        );
        let out = self.run(&[
            "-S",
            &binding::fd_path(&t.socket_fd),
            "if-shell",
            "-F",
            "-t",
            &format!("={}:", self.session),
            &guard,
            &yes,
            "display-message -p wfc-tmux-refused",
        ])?;
        if out.status.success() {
            let output = String::from_utf8_lossy(&out.stdout);
            if let Some(data) = output
                .strip_prefix(&format!("WFC_STATE={}\n", t.owner))
                .and_then(|data| data.strip_suffix("wfc-tmux-confirmed\n"))
            {
                return Ok(data.to_owned());
            }
            if output == "wfc-tmux-refused\n" {
                return Err(failure("refused", "tmux session binding changed"));
            }
        }
        Err(failure("uncertain", "tmux did not confirm the operation"))
    }

    fn pane_command(&self, t: &Target, action: &str) -> Reply<String> {
        // A window linked into another session shares its pane contents and keystrokes.
        self.guarded(t, action, EXCLUSIVE_WINDOW)
    }

    fn capture(&self, target: &Target) -> Reply<String> {
        self.pane_command(target, &format!("capture-pane -p -t {}", target.pane))
    }

    fn close(&self) -> Reply<Value> {
        if let Some(t) = self.target()? {
            if !t.owned {
                return Err(failure(
                    "refused",
                    "session ownership does not match WFC_STATE",
                ));
            }
            // Killing a session leaves linked windows in their other sessions.
            // Check all windows, even when the active window is exclusive.
            let exclusive = "#{m/r:^1+$,#{W:#{?#{==:#{window_linked_sessions},1},1,0}}}";
            let has_worker = format!("#{{m:*|{}|*,#{{W:#{{P:|#{{pane_id}}|}}}}}}", t.worker_pane);
            self.guarded(
                &t,
                &format!("kill-session -t '{}'", t.session),
                &format!("#{{&&:{exclusive},{has_worker}}}"),
            )?;
        }
        self.forget()?;
        Ok(json!({"result":"completed"}))
    }
}

fn valid_dispatch(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(crate) fn attach(state: &Path, dispatch: &str, socket: &Path, token: &str) -> Result<()> {
    ensure!(
        valid_dispatch(dispatch)
            && token.len() == 32
            && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid attach identity"
    );
    let state = state.canonicalize()?;
    let backend = Tmux {
        state: &state,
        dispatch,
        session: format!("wfc-{dispatch}"),
        socket: Some(socket.to_owned()),
    };
    let lock = backend
        .lock_dispatch()
        .map_err(|_| anyhow::anyhow!("cannot verify tmux attach claim"))?;
    ensure!(
        backend.claim().is_ok_and(|c| c.is_some()),
        "tmux attach claim expired"
    );
    let t = backend
        .owned()
        .map_err(|_| anyhow::anyhow!("tmux attach target is unavailable"))?;
    ensure!(t.token == token, "tmux attach target changed");
    drop(lock);
    // Keep the verified socket inode pinned for the client. Recheck the
    // session, pane and explicit owner in the server queue at actual attach.
    let attach = format!(
        "show-environment -t '{0}' WFC_STATE ; attach-session -E -t '{0}'",
        t.session
    );
    let condition = backend.guard(&t, EXCLUSIVE_WINDOW);
    let status = backend.command()
        .args(["-N", "-S", &binding::fd_path(&t.socket_fd), "if-shell", "-F", "-t"])
        .arg(format!("={}:", backend.session))
        .arg(condition)
        .arg(attach)
        .arg("display-message -p 'tmux attach refused: session binding changed' ; run-shell 'exit 1'")
        .status()?;
    ensure!(status.success(), "tmux attach refused or disconnected");
    Ok(())
}

pub fn execute(state: &Path, request: Request) -> Result<Value> {
    ensure!(
        matches!(request.backend.as_deref(), None | Some("" | "tmux")),
        "request selects another execution backend"
    );
    ensure!(valid_dispatch(&request.dispatch), "invalid dispatch ID");
    if matches!(request.op, Operation::Create(_) | Operation::Resume { .. }) {
        fs::create_dir_all(state).context("create tmux state directory")?;
    }
    let state = if state.exists() {
        state.canonicalize()?
    } else {
        std::path::absolute(state)?
    };
    let backend = Tmux {
        state: &state,
        dispatch: &request.dispatch,
        session: format!("wfc-{}", request.dispatch),
        socket: env::var_os("WFC_TMUX_SOCKET")
            .map(std::path::absolute)
            .transpose()?,
    };
    let _lock = match backend.lock_dispatch() {
        Ok(lock) => lock,
        Err(result) => return Ok(result),
    };
    let result = (|| match request.op {
        Operation::Inspect => backend.inspect(),
        Operation::Read => {
            Ok(json!({"result":"accepted","screen":backend.capture(&backend.owned()?)?}))
        }
        Operation::Interrupt => {
            let target = backend.owned()?;
            backend.pane_command(&target, &format!("send-keys -t {} C-c", target.pane))?;
            Ok(json!({"result":"accepted"}))
        }
        Operation::Close => backend.close(),
        Operation::Create(launch) => backend
            .create(launch)
            .map(|(_, receipt)| json!({"result":"accepted","receipt":receipt})),
        Operation::Submit { text, host } => backend
            .owned()
            .and_then(|target| backend.submit(&target, &text, host.as_deref()))
            .map_err(|mut error| {
                if error["result"] == "definitely-not-sent" {
                    error["category"] = json!("input_not_delivered");
                }
                error
            }),
        Operation::Resume {
            launch,
            host,
            text,
            probe_read_only,
        } => {
            let (target, receipt) = backend.create(launch).map_err(|mut e| {
                if host.as_deref() == Some("opencode") {
                    // OpenCode receives this brief only after create returns a verified target.
                    e["inputResult"] = json!("definitely-not-sent");
                } else if host.as_deref() == Some("codex") && probe_read_only {
                    e["inputResult"] = json!(if e["result"] == "definitely-not-sent"
                        || e["result"] == "refused"
                    {
                        "definitely-not-sent"
                    } else {
                        "uncertain"
                    });
                }
                e
            })?;
            let mut result = backend
                .resume_input(&target, host.as_deref(), text.as_deref(), probe_read_only)
                .unwrap_or_else(|e| e);
            result["launchResult"] = json!("accepted");
            if host.as_deref() == Some("opencode")
                || (host.as_deref() == Some("codex") && probe_read_only)
            {
                result["inputResult"] = result["result"].clone();
            }
            if result["result"] != "accepted" && result["category"] != "permission_required" {
                result["result"] = json!("uncertain");
            }
            result["receipt"] = json!(receipt);
            Ok(result)
        }
    })()
    .unwrap_or_else(|error| error);
    Ok(result)
}
