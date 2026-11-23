//! Linux tmux execution seam. The workflow owns attempts, serialization of lifecycle
//! operations, and delivery receipts. Each call can run in a fresh CLI process.
mod input;
pub(crate) mod launch;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs;
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

struct Target {
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
        command.arg("-u").env("LC_ALL", "C");
        for (key, _) in env::vars_os() {
            if launch::router_credential(&key.to_string_lossy()) {
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
        let selector = format!("={}:", self.session);
        let found = self.run(&[
            "list-panes",
            "-t",
            &selector,
            "-f",
            "#{pane_active}",
            "-F",
            "#{session_id} #{pane_id} #{pid}",
        ])?;
        if !found.status.success() {
            let err = String::from_utf8_lossy(&found.stderr);
            if err.trim_end() == format!("can't find session: {}", self.session)
                || err.starts_with("no server running on ")
                || (err.starts_with("error connecting to ")
                    && err.ends_with(" (No such file or directory)\n"))
            {
                return Ok(None);
            }
            return Err(failure("uncertain", "tmux could not inspect the session"));
        }
        let ids = String::from_utf8_lossy(&found.stdout);
        let fields: Vec<_> = ids.split_whitespace().collect();
        let [session, pane, server_pid] = fields.as_slice() else {
            return Err(failure("uncertain", "invalid tmux target identity"));
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !session.strip_prefix('$').is_some_and(digits)
            || !pane.strip_prefix('%').is_some_and(digits)
            || !digits(server_pid)
        {
            return Err(failure("uncertain", "invalid tmux target identity"));
        }
        let shown = self.run(&["show-environment", "-t", session, "WFC_STATE"])?;
        let owner = std::str::from_utf8(&shown.stdout)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .and_then(|s| s.strip_prefix("WFC_STATE="))
            .unwrap_or("");
        let owned = shown.status.success()
            && Path::new(owner).is_absolute()
            && fs::canonicalize(owner).is_ok_and(|p| p == self.state);
        Ok(Some(Target {
            session: session.to_string(),
            pane: pane.to_string(),
            server_pid: server_pid.to_string(),
            owner: owner.to_owned(),
            owned,
        }))
    }

    fn attach(&self, session_id: &str) -> Vec<String> {
        let mut argv = vec!["tmux".to_owned()];
        if let Some(socket) = &self.socket {
            argv.extend(["-S".to_owned(), socket.display().to_string()]);
        }
        argv.extend(["attach-session".into(), "-t".into(), session_id.to_owned()]);
        argv
    }

    fn inspect(&self) -> Reply<Value> {
        Ok(match self.target()? {
            None => json!({"result":"accepted","alive":false,"owned":false}),
            Some(t) if t.owned => json!({"result":"accepted","alive":true,"owned":true,
                "session":self.session, "attach":self.attach(&t.session)}),
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
    fn guarded(&self, t: &Target, action: &str, condition: &str) -> Reply<String> {
        let owner = t
            .owner
            .replace('#', "##")
            .replace(',', "#,")
            .replace('}', "#}");
        let server = format!("#{{==:#{{pid}},{}}}", t.server_pid);
        let session = format!("#{{==:#{{session_id}},{}}}", t.session);
        let owner = format!("#{{==:#{{WFC_STATE}},{owner}}}");
        let guard = format!("#{{&&:{server},#{{&&:{session},#{{&&:{owner},{condition}}}}}}}");
        let yes = format!("{action} ; display-message -p wfc-tmux-confirmed");
        let out = self.run(&[
            "if-shell",
            "-F",
            "-t",
            &t.pane,
            &guard,
            &yes,
            "display-message -p wfc-tmux-refused",
        ])?;
        if out.status.success() {
            let output = String::from_utf8_lossy(&out.stdout);
            if let Some(data) = output.strip_suffix("wfc-tmux-confirmed\n") {
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
        self.guarded(t, action, "#{==:#{window_linked},0}")
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
            self.guarded(&t, &format!("kill-session -t '{}'", t.session), "1")?;
        }
        Ok(json!({"result":"completed"}))
    }
}

pub fn execute(state: &Path, request: Request) -> Result<Value> {
    ensure!(
        matches!(request.backend.as_deref(), None | Some("" | "tmux")),
        "request selects another execution backend"
    );
    ensure!(
        !request.dispatch.is_empty()
            && request
                .dispatch
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "invalid dispatch ID"
    );
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
        socket: env::var_os("WFC_TMUX_SOCKET").map(PathBuf::from),
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
            let (target, receipt) = backend.create(launch)?;
            let mut result = backend
                .resume_input(&target, host.as_deref(), text.as_deref(), probe_read_only)
                .unwrap_or_else(|e| e);
            result["receipt"] = json!(receipt);
            Ok(result)
        }
    })()
    .unwrap_or_else(|error| error);
    Ok(result)
}
