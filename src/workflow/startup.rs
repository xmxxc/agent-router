//! Publish typed startup evidence. Child output and error strings are not forwarded.
use serde::Serialize;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

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
        self.publish(Failure {
            category,
            diagnostic,
            exit_code: None,
            signal: None,
            launch_id: self.launch_id.as_deref(),
            pid: None,
            process_start: None,
            requires_decision: true,
        });
    }

    pub(crate) fn watch(&self, pid: u32) -> Running<'_> {
        Running {
            startup: self,
            since: Instant::now(),
            pid,
            process_start: wfc_herdr::Process::observe(pid)
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

pub(crate) struct Running<'a> {
    startup: &'a Startup,
    since: Instant,
    pid: u32,
    process_start: Option<String>,
}

impl Running<'_> {
    pub(crate) fn exited(self, status: ExitStatus) {
        if status.success() || self.since.elapsed() > Duration::from_millis(300) {
            return;
        }
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
    use serde_json::{json, Value};

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
            let history = super::super::messages::history(&state, dispatch)?;
            assert_eq!(history.len(), 2);
            assert_eq!(history[1]["type"], "question");
            assert_eq!(history[1]["question"], "Can I proceed?");
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(state);
        result
    }
}
