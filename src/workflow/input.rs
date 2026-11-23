//! Public input to the existing worker, with round changes committed before typing.
use super::{assignment, backend, controller, messages, record_scope, state_dir, view, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::ErrorKind,
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const CONTINUE_USAGE: &str =
    "wfc continue DISPATCH --task FILE --report FILE [--owner-token TOKEN]";
pub const NUDGE_USAGE: &str = "wfc nudge DISPATCH --text TEXT [--owner-token TOKEN]";

pub fn usage(name: &str) -> &'static str {
    if name == "continue" {
        CONTINUE_USAGE
    } else {
        NUDGE_USAGE
    }
}

enum Action<'a> {
    Continue { task: &'a Path, report: &'a Path },
    Nudge(&'a str),
}

impl Action<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Continue { .. } => "continue",
            Self::Nudge(_) => "nudge",
        }
    }
}

pub struct Request<'a> {
    dispatch: &'a str,
    token: Option<&'a str>,
    action: Action<'a>,
}

impl<'a> Request<'a> {
    pub fn parse(name: &str, args: &'a [OsString]) -> Result<Self> {
        let mut dispatch = None;
        let (token, action) = if name == "continue" {
            let [token, task, report] = messages::options(
                args,
                ["--owner-token", "--task", "--report"],
                Some((&mut dispatch, "dispatch ID")),
            )?;
            (
                token,
                Action::Continue {
                    task: Path::new(task.context("missing --task")?),
                    report: Path::new(report.context("missing --report")?),
                },
            )
        } else {
            let [token, text] = messages::options(
                args,
                ["--owner-token", "--text"],
                Some((&mut dispatch, "dispatch ID")),
            )?;
            (token, Action::Nudge(text.context("missing --text")?))
        };
        let dispatch = dispatch
            .filter(|id| super::valid_id(id))
            .context("invalid or missing dispatch ID")?;
        Ok(Self {
            dispatch,
            token,
            action,
        })
    }

    pub fn run(self) -> Result<Value> {
        let operation = self.action.name();
        let state = state_dir()?;
        let dir = state.join("dispatches");
        let path = dir.join(format!("{}.json", self.dispatch));
        let original = assignment::public_record(&dir, self.dispatch)?
            .with_context(|| format!("unknown dispatch {}", self.dispatch))?;
        ensure!(
            original["backend"] == "tmux" && original["host"] == "codex",
            "{operation} requires a supported live Codex/tmux assignment"
        );
        let scope = record_scope(&original).context("invalid dispatch scope")?;
        let token = controller::token(self.token);
        let (_scope, _execution) =
            lock_input(&state, &original, scope, token.as_deref(), operation)?;
        let current = super::read_record(&path)?;
        ensure!(
            record_scope(&current) == Some(scope)
                && [
                    "round",
                    "attempt",
                    "host",
                    "backend",
                    "executionId",
                    "tmuxSocket",
                    "create"
                ]
                .iter()
                .all(|key| current[*key] == original[*key]),
            "assignment changed while waiting for input"
        );
        controller::Owners::read(&state)?.require(scope, token.as_deref(), operation)?;
        ensure!(
            current["controllerOwner"].is_null()
                && assignment::identified(&current)
                && current["executionId"].is_string()
                && current["create"]["result"] == "accepted",
            "input requires a confirmed native launch"
        );
        ensure!(
            !assignment::input_unresolved(&current),
            "input is pending or uncertain; verify the prior input before retrying"
        );
        ensure!(
            backend::archive(&state, &current)?.is_none(),
            "execution resources are closing or closed; use resume for an exited worker"
        );
        let (worker, observed) = backend::observe(&state, &mut current.clone())?;
        ensure!(
            observed["result"] == "accepted",
            "could not verify the live execution: {}",
            observed["diagnostic"]
        );
        ensure!(
            observed["alive"] == true,
            "execution backend reports the worker is not running; use resume instead"
        );
        ensure!(
            observed["owned"] == true
                && observed["workerBound"] == true
                && worker["state"] == "running",
            "execution is not a verified writable worker"
        );

        let (mut next, index, text) = {
            let _inbox = messages::lock_for_publish(&state)?;
            controller::Owners::read(&state)?.require(scope, token.as_deref(), operation)?;
            let history =
                view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
            let mut projected = current.clone();
            view::project(&mut projected, &history, Some(true), false)?;
            let mut next = current.clone();
            let mut event = json!({"id":format!("i-{}", Uuid::new_v4().simple()),
                "operation":operation, "result":"pending"});
            let text = match self.action {
                Action::Continue { task, report } => {
                    ensure!(
                        projected["state"] == "done",
                        "dispatch has not done its current task yet"
                    );
                    let (task, report) = next_paths(&current, &projected, &history, task, report)?;
                    ensure!(
                        current[controller::CALLBACK_HASH]
                            .as_str()
                            .is_some_and(|hash| !hash.is_empty()),
                        "original worker has no independent callback credential"
                    );
                    let round = current["round"]
                        .as_u64()
                        .and_then(|value| value.checked_add(1))
                        .context("invalid next round")?;
                    let attempt = format!("a-{}", Uuid::new_v4().simple());
                    let reference =
                        format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
                    let created = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
                    next["rounds"] = projected["rounds"].clone();
                    next["rounds"].as_array_mut().context("invalid round history")?.push(json!({
                        "round":round,"attempt":attempt,"task":task,"report":report,"created":created,
                        "completed":null,"state":"running","result":null
                    }));
                    next["round"] = json!(round);
                    next["attempt"] = json!(attempt);
                    next["task"] = json!(task);
                    next["report"] = json!(report);
                    next[controller::CALLBACK_REF_HASH] = json!(messages::body_hash(&reference));
                    event["task"] = json!(task);
                    event["report"] = json!(report);
                    event["fromAttempt"] = current["attempt"].clone();
                    continued_brief(self.dispatch, &task, &report, &attempt, &reference)
                }
                Action::Nudge(text) => {
                    ensure!(
                        projected["state"] != "done",
                        "dispatch already finished its task; give the next one with continue"
                    );
                    ensure!(
                        matches!(projected["state"].as_str(), Some("working" | "asking")),
                        "current round is not writable"
                    );
                    let text = text
                        .replace("\r\n", "\n")
                        .split_terminator([
                            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}',
                            '\u{2028}', '\u{2029}',
                        ])
                        .collect::<Vec<_>>()
                        .join(" ");
                    ensure!(
                        !text.trim().is_empty() && !text.chars().any(char::is_control),
                        "invalid nudge text"
                    );
                    event["text"] = json!(text);
                    format!("[router] Message from the controller: {text}")
                }
            };
            event["round"] = next["round"].clone();
            event["attempt"] = next["attempt"].clone();
            if next["audit"].is_null() {
                next["audit"] = json!([]);
            }
            let audit = next["audit"]
                .as_array_mut()
                .context("invalid input audit")?;
            let index = audit.len();
            audit.push(event);
            save(&path, &dir, &next)?;
            (next, index, text)
        };

        let answer = backend::submit(&state, &current, &text).unwrap_or_else(
            |error| json!({"result":"uncertain","diagnostic":format!("{error:#}")}),
        );
        let diagnostic = answer["diagnostic"]
            .as_str()
            .unwrap_or("backend did not confirm input");
        let mut outcome = match (answer["result"].as_str(), answer["inputResult"].as_str()) {
            (Some("accepted"), _) => "accepted",
            (Some("refused"), _) => "refused",
            (Some("definitely-not-sent"), _) | (_, Some("definitely-not-sent")) => {
                "definitely-not-sent"
            }
            _ => "uncertain",
        };
        let _inbox = messages::lock_for_publish(&state)?;
        let saved = super::read_record(&path)?;
        ensure!(
            saved["attempt"] == next["attempt"]
                && saved["round"] == next["round"]
                && saved["audit"][index] == next["audit"][index],
            "assignment changed before input settlement; verify the recorded input"
        );
        let mut event = next["audit"][index].clone();
        if operation == "continue" && matches!(outcome, "refused" | "definitely-not-sent") {
            if messages::history(&state, self.dispatch)?
                .iter()
                .any(|m| m["round"] == next["round"] && m["attempt"] == next["attempt"])
            {
                // A new callback is direct evidence that the new identity was
                // in use. Never roll it back even if the backend says zero input.
                outcome = "uncertain";
                event["diagnostic"] =
                    json!("a new-round callback conflicts with the backend result");
            } else {
                next = current;
            }
        }
        event["result"] = json!(outcome);
        if outcome != "accepted" && event["diagnostic"].is_null() {
            event["diagnostic"] = json!(diagnostic);
        }
        if next["audit"].is_null() {
            next["audit"] = json!([]);
        }
        let audit = next["audit"]
            .as_array_mut()
            .context("invalid input audit")?;
        if index == audit.len() {
            audit.push(event.clone());
        } else {
            audit[index] = event.clone();
        }
        save(&path, &dir, &next)?;
        if outcome != "accepted" {
            bail!("{operation} {outcome}: {}", event["diagnostic"]);
        }
        let mut result = json!({"dispatch":self.dispatch, "operation":operation, "result":outcome,
            "round":next["round"], "attempt":next["attempt"], "audit":event["id"]});
        let action = if operation == "continue" {
            "continued"
        } else {
            "nudged"
        };
        result[action] = json!(self.dispatch);
        Ok(result)
    }
}

fn lock_input(
    state: &Path,
    row: &Value,
    scope: &str,
    token: Option<&str>,
    operation: &str,
) -> Result<(File, File)> {
    let start = Instant::now();
    loop {
        // Same scope -> execution -> inbox order as close and #12 reply.
        if let Some(scope_lock) = controller::try_scope_lock(state, scope)? {
            controller::Owners::read(state)?.require(scope, token, operation)?;
            if let Some(execution) = assignment::try_reply_lock(state, row)? {
                return Ok((scope_lock, execution));
            }
        }
        let remaining = Duration::from_secs(30).saturating_sub(start.elapsed());
        ensure!(!remaining.is_zero(), "input_lock_timeout");
        sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn save(path: &Path, dir: &Path, row: &Value) -> Result<()> {
    write_json(path, row, None)?;
    messages::sync_directory(dir)
}

fn next_paths(
    current: &Value,
    projected: &Value,
    history: &[Value],
    task: &Path,
    report: &Path,
) -> Result<(String, String)> {
    let task = task.canonicalize().context("resolve task file")?;
    ensure!(task.is_file(), "--task must name a file");
    let original = Path::new(
        current["report"]
            .as_str()
            .context("missing original report")?,
    );
    let absolute = std::path::absolute(report)?;
    let parent = absolute
        .parent()
        .context("missing report directory")?
        .canonicalize()
        .context("resolve report directory")?;
    ensure!(
        original.parent() == Some(parent.as_path()),
        "next report must be in the original report directory"
    );
    let report = parent.join(absolute.file_name().context("missing report filename")?);
    let report = path_text(report)?;
    ensure!(
        current["report"] != report
            && !projected["rounds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r["report"] == report)
            && !history.iter().any(|m| m["report"] == report),
        "report was already used by an earlier round"
    );
    match fs::symlink_metadata(&report) {
        Ok(_) => bail!("next report already exists"),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("check next report path"),
    }
    Ok((path_text(task)?, report))
}

fn path_text(path: PathBuf) -> Result<String> {
    let text = path
        .into_os_string()
        .into_string()
        .ok()
        .context("path must be UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "path contains a control character"
    );
    Ok(text)
}

fn continued_brief(
    dispatch: &str,
    task: &str,
    report: &str,
    attempt: &str,
    reference: &str,
) -> String {
    let task = task.replace('\'', "'\\''");
    let report = report.replace('\'', "'\\''");
    format!("[router] New task for you (dispatch {dispatch}). Read and do the task in '{task}'. \
        Stop any server or background process you started before reporting. Write the report to '{report}'. \
        Run \"$WFC_BIN\" done --report '{report}' --attempt {attempt} --callback-ref {reference}. \
        If you need a decision, run \"$WFC_BIN\" ask \"<question>\" --attempt {attempt} \
        --callback-ref {reference} and end your turn.")
}
