//! Publish typed startup evidence. Child output and error strings are not forwarded.
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

const STARTUP_WINDOW: Duration = Duration::from_millis(300);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Failure<'a> {
    category: &'static str,
    diagnostic: &'static str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    launch_id: Option<&'a str>,
    pid: Option<u32>,
    process_start: Option<&'a str>,
    requires_decision: bool,
}

impl Failure<'_> {
    pub(super) fn diagnostic(&self) -> &'static str {
        self.diagnostic
    }

    pub(super) fn message_type(&self) -> &'static str {
        match self.category {
            "process_exited" => "exited",
            "executable_missing" | "permission_denied" => "blocked",
            _ => "unknown",
        }
    }
}

pub(crate) struct Startup {
    state: PathBuf,
    dispatch: String,
    attempt: String,
    launch_id: Option<String>,
}

impl Startup {
    pub(crate) fn new(
        state: &Path,
        dispatch: &str,
        attempt: &str,
        launch_id: Option<&str>,
    ) -> Self {
        Self {
            state: state.to_owned(),
            dispatch: dispatch.to_owned(),
            attempt: attempt.to_owned(),
            launch_id: launch_id.map(str::to_owned),
        }
    }

    // Private backend commands may be used without a public assignment.
    pub(crate) fn hosted(
        env: &std::collections::BTreeMap<String, String>,
        launch_id: &str,
    ) -> Option<Self> {
        Some(Self::new(
            Path::new(env.get("WFC_STATE")?),
            env.get("WFC_DISPATCH")?,
            env.get("WFC_ATTEMPT")?,
            Some(launch_id),
        ))
    }

    pub(crate) fn spawn_failed(&self, kind: ErrorKind) {
        let (category, diagnostic) = match kind {
            ErrorKind::NotFound => (
                "executable_missing",
                "Worker executable or interpreter was not found",
            ),
            ErrorKind::PermissionDenied => (
                "permission_denied",
                "Permission denied while starting the worker",
            ),
            _ => ("spawn_failed", "Worker could not start"),
        };
        self.publish(self.not_started(category, diagnostic));
    }

    pub(super) fn script_unavailable(
        &self,
        error: super::host::ScriptUnavailable,
        ledger: std::fs::File,
    ) {
        let (category, diagnostic) = match error {
            super::host::ScriptUnavailable::Missing => (
                "executable_missing",
                "Configured worker script was not found",
            ),
            super::host::ScriptUnavailable::PermissionDenied => (
                "permission_denied",
                "Permission denied while resolving the configured worker script",
            ),
        };
        let failure = self.not_started(category, diagnostic);
        if super::messages::publish_startup_failure_locked(
            &self.state,
            &self.dispatch,
            &self.attempt,
            &failure,
            ledger,
        )
        .is_err()
        {
            eprintln!("wfc: could not save worker startup notification");
        }
    }

    fn not_started(&self, category: &'static str, diagnostic: &'static str) -> Failure<'_> {
        Failure {
            category,
            diagnostic,
            exit_code: None,
            signal: None,
            launch_id: self.launch_id.as_deref(),
            pid: None,
            process_start: None,
            requires_decision: true,
        }
    }

    pub(crate) fn watch(&self, child: &Child) -> Running<'_> {
        Running {
            startup: self,
            since: Instant::now(),
            pid: child.id(),
            process_start: wfc_herdr::Process::observe_unreaped_child(child)
                .ok()
                .flatten()
                .map(|p| p.creation),
        }
    }

    fn publish(&self, failure: Failure<'_>) {
        if super::messages::publish_startup_failure(
            &self.state,
            &self.dispatch,
            &self.attempt,
            &failure,
        )
        .is_err()
        {
            eprintln!("wfc: could not save worker startup notification");
        }
    }
}

// Started receipts can block on storage. Observe the exit independently and
// finish the started write before the caller publishes terminal evidence.
pub(crate) fn wait_host(
    child: &mut Child,
    save_started: impl FnOnce() -> Result<()> + Send,
) -> Result<(ExitStatus, Instant, Result<()>)> {
    let (status, observed, started) = std::thread::scope(|scope| {
        let writer = std::thread::Builder::new().spawn_scoped(scope, save_started);
        let status = child.wait();
        let observed = Instant::now();
        let started = match writer {
            Ok(writer) => writer
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("host startup receipt writer failed"))),
            Err(err) => Err(err.into()),
        };
        (status, observed, started)
    });
    Ok((status?, observed, started))
}

// A native zero exit is a startup fault only when the public host adapter
// actually rejected confirmation and selected this terminal receipt.
pub(super) fn unconfirmed_zero(state: &Path, row: &Value, launch_id: &str, receipt: &Value) {
    if receipt["category"] != "process_exited"
        || receipt["exitCode"] != 0
        || !receipt["startupZero"]["pid"]
            .as_u64()
            .is_some_and(|pid| pid > 0 && pid <= u32::MAX as u64)
    {
        return;
    }
    publish_unconfirmed_exit(
        state,
        row,
        launch_id,
        &receipt["startupZero"],
        Some(0),
        None,
    );
}

pub(super) fn unconfirmed_tmux_exit(
    state: &Path,
    row: &Value,
    launch_id: &str,
    receipt: &Value,
) -> Result<()> {
    if let Some(failure) = tmux_failure(launch_id, receipt)? {
        let Some((dispatch, attempt)) = row["dispatch"].as_str().zip(row["attempt"].as_str())
        else {
            return Ok(());
        };
        super::messages::publish_startup_failure(state, dispatch, attempt, &failure)?;
    }
    Ok(())
}

pub(super) fn tmux_failure<'a>(
    launch_id: &'a str,
    receipt: &'a Value,
) -> Result<Option<Failure<'a>>> {
    let Some(evidence) = receipt.get("startupExit") else {
        // Older supervisors recorded raw identity only for an unconfirmed zero.
        let zero = &receipt["startupZero"];
        return Ok((receipt["category"] == "process_exited"
            && receipt["exitCode"] == 0
            && zero["pid"]
                .as_u64()
                .is_some_and(|pid| pid > 0 && pid <= u32::MAX as u64))
        .then(|| unconfirmed_failure(launch_id, zero, Some(0), None)));
    };
    let (exit_code, signal, encoded) =
        match (evidence["exitCode"].as_i64(), evidence["signal"].as_i64()) {
            (Some(code @ 0..=255), None) if evidence["signal"].is_null() => {
                (Some(code as i32), None, code)
            }
            (None, Some(signal @ 1..=64)) if evidence["exitCode"].is_null() => {
                (None, Some(signal as i32), 128 + signal)
            }
            _ => anyhow::bail!("invalid tmux startup exit evidence"),
        };
    anyhow::ensure!(
        receipt["exitCode"] == encoded,
        "tmux startup exit evidence does not match receipt"
    );
    Ok(Some(unconfirmed_failure(
        launch_id, evidence, exit_code, signal,
    )))
}

fn publish_unconfirmed_exit(
    state: &Path,
    row: &Value,
    launch_id: &str,
    evidence: &Value,
    exit_code: Option<i32>,
    signal: Option<i32>,
) {
    let Some((dispatch, attempt)) = row["dispatch"].as_str().zip(row["attempt"].as_str()) else {
        return;
    };
    Startup::new(state, dispatch, attempt, Some(launch_id))
        .publish(unconfirmed_failure(launch_id, evidence, exit_code, signal));
}

fn unconfirmed_failure<'a>(
    launch_id: &'a str,
    evidence: &'a Value,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> Failure<'a> {
    let pid = evidence["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|&pid| pid > 0);
    Failure {
        category: "process_exited",
        diagnostic: "Worker exited before host startup was confirmed",
        exit_code,
        signal,
        launch_id: Some(launch_id),
        pid,
        process_start: evidence["processStart"].as_str(),
        requires_decision: true,
    }
}

pub(crate) struct Running<'a> {
    startup: &'a Startup,
    since: Instant,
    pid: u32,
    process_start: Option<String>,
}

impl Running<'_> {
    fn during_startup(&self, observed: Instant) -> bool {
        observed.saturating_duration_since(self.since) <= STARTUP_WINDOW
    }

    pub(crate) fn exited(self, status: ExitStatus, observed: Instant) {
        if !status.success() && self.during_startup(observed) {
            self.publish_exit(status);
        }
    }

    pub(crate) fn host_exited(self, status: ExitStatus, observed: Instant) -> Option<Value> {
        if !self.during_startup(observed) {
            return None;
        }
        if status.success() {
            return Some(json!({"pid":self.pid,"processStart":self.process_start}));
        }
        self.publish_exit(status);
        None
    }

    pub(crate) fn tmux_exit_evidence(self, status: ExitStatus) -> Value {
        // Only create can tell whether this launch passed final confirmation.
        json!({"pid":self.pid,"processStart":self.process_start,
            "exitCode":status.code(),"signal":status.signal()})
    }

    fn publish_exit(&self, status: ExitStatus) {
        self.startup.publish(Failure {
            category: "process_exited",
            diagnostic: "Worker exited during startup",
            exit_code: status.code(),
            signal: status.signal(),
            launch_id: self.startup.launch_id.as_deref(),
            pid: Some(self.pid),
            process_start: self.process_start.as_deref(),
            requires_decision: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn unreaped_exit_keeps_creation_identity_across_a_slow_started_receipt() -> Result<()> {
        let state = std::env::temp_dir().join(format!("wfc-startup-{}", uuid::Uuid::new_v4()));
        let dispatch = "d-aabbccdd";
        let attempt = "a-123456789abc";
        let result = (|| -> Result<()> {
            let records = state.join("dispatches");
            std::fs::create_dir_all(&records)?;
            super::super::write_json(
                &records.join(format!("{dispatch}.json")),
                &json!({
                    "dispatch":dispatch,"attempt":attempt,"round":1,"role":"fixture",
                    "scope":"slow-started-receipt","cwd":state,"report":state.join("report"),
                }),
                None,
            )?;

            let mut child = Command::new("/bin/sh")
                .args(["-c", "read -r line; exit 23"])
                .stdin(Stdio::piped())
                .spawn()?;
            let pid = child.id();
            let live = wfc_herdr::Process::observe(pid);
            drop(child.stdin.take());
            // Wait for the terminal event without reaping or releasing the PID.
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            } == -1
            {
                let err = std::io::Error::last_os_error();
                let _ = child.wait();
                return Err(err.into());
            }
            let exited = wfc_herdr::Process::observe(pid);
            let startup = Startup::new(&state, dispatch, attempt, Some("fixture-launch"));
            let watching = startup.watch(&child);
            let written = std::sync::OnceLock::new();
            let (status, observed, started) = wait_host(&mut child, || {
                std::thread::sleep(STARTUP_WINDOW + Duration::from_millis(25));
                written.set(Instant::now()).unwrap();
                Ok(())
            })?;
            started?;
            let live = live?.expect("child was alive before stdin was closed");
            assert_eq!(exited?, None);
            assert_eq!(status.code(), Some(23));
            assert!(observed < *written.get().unwrap());
            assert!(watching.host_exited(status, observed).is_none());
            let history = super::super::messages::history(&state, dispatch)?;
            assert_eq!(history.len(), 1);
            assert_eq!(history[0]["fault"]["exitCode"], 23);
            assert_eq!(history[0]["fault"]["pid"], pid);
            assert_eq!(history[0]["fault"]["processStart"], live.creation);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(state);
        result
    }

    #[test]
    fn startup_publication_uses_the_current_attempt_and_one_durable_key() -> anyhow::Result<()> {
        let state = std::env::temp_dir().join(format!("wfc-startup-{}", uuid::Uuid::new_v4()));
        let records = state.join("dispatches");
        let dispatch = "d-aabbccdd";
        let attempt = "a-123456789abc";
        let startup = Startup::new(&state, dispatch, attempt, None);
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(&records)?;
            let path = records.join(format!("{dispatch}.json"));
            let mut row = json!({"dispatch":dispatch, "attempt":attempt, "round":1,
                "role":"fixture", "scope":"only-this-scope", "cwd":state, "report":state.join("report")});
            super::super::write_json(&path, &row, None)?;
            std::thread::scope(|scope| {
                scope.spawn(|| startup.spawn_failed(ErrorKind::NotFound));
                scope.spawn(|| startup.spawn_failed(ErrorKind::NotFound));
            });
            let second_client = Startup::new(&state, dispatch, attempt, None);
            second_client.spawn_failed(ErrorKind::NotFound);
            let history = super::super::messages::history(&state, dispatch)?;
            assert_eq!(history.len(), 1);
            assert_eq!(history[0]["scope"], "only-this-scope");
            assert_eq!(history[0]["attempt"], attempt);
            assert_eq!(history[0]["fault"]["category"], "executable_missing");

            row["round"] = json!(2);
            row["attempt"] = json!("a-987654321abc");
            super::super::write_json(&path, &row, None)?;
            second_client.spawn_failed(ErrorKind::NotFound);
            assert_eq!(super::super::messages::history(&state, dispatch)?.len(), 1);

            let mut messages: Vec<Value> =
                serde_json::from_reader(std::fs::File::open(state.join("messages.json"))?)?;
            messages.push(json!({"id":2,"messageId":"m-question","dispatch":dispatch,
                "attempt":row["attempt"],"round":2,"role":"fixture","scope":"only-this-scope",
                "state":"received","statusVersion":2,
                "question_payload":{"text":"Can I proceed?","options":[]}}));
            super::super::write_json(&state.join("messages.json"), &json!(messages), None)?;
            Startup::new(&state, dispatch, "a-987654321abc", None)
                .spawn_failed(ErrorKind::NotFound);
            unconfirmed_tmux_exit(
                &state,
                &row,
                "fixture-launch",
                &json!({
                    "category":"process_exited","exitCode":17,
                    "startupExit":{"exitCode":17,"signal":null,"pid":1,"processStart":null},
                }),
            )?;
            let history = super::super::messages::history(&state, dispatch)?;
            assert_eq!(history.len(), 2);
            assert_eq!(history[1]["type"], "question");
            assert_eq!(history[1]["question"], "Can I proceed?");

            row["round"] = json!(3);
            row["attempt"] = json!("a-123abc789456");
            super::super::write_json(&path, &row, None)?;
            unconfirmed_zero(
                &state,
                &row,
                "fixture-launch",
                &json!({
                    "category":"process_exited","exitCode":0,"startupZero":{"pid":1,"processStart":null},
                }),
            );
            let history = super::super::messages::history(&state, dispatch)?;
            assert_eq!(history.len(), 3);
            assert_eq!(history[2]["fault"]["exitCode"], 0);
            assert!(history[2]["fault"]["processStart"].is_null());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(state);
        result
    }
}
