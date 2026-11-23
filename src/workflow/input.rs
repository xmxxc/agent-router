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
        controller::Owners::read(&state)?.require(scope, token.as_deref(), operation)?;
        let nudge = match &self.action {
            Action::Nudge(text) => Some(nudge_text(text)?),
            _ => None,
        };
        let original_unresolved = assignment::input_unresolved(&original);
        ensure!(
            !original_unresolved
                || replay_index(&original, &self.action, nudge.as_deref())
                    .is_some_and(|index| original["audit"][index]["result"] == "pending"),
            "input is pending or uncertain; verify the prior input before retrying"
        );
        let (_scope, _execution) =
            lock_input(&state, &original, scope, token.as_deref(), operation)?;
        let mut current = super::read_record(&path)?;
        ensure!(
            record_scope(&current) == Some(scope),
            "scope changed while waiting for input"
        );
        controller::Owners::read(&state)?.require(scope, token.as_deref(), operation)?;
        ensure!(
            ["host", "backend", "executionId", "tmuxSocket", "create"]
                .iter()
                .all(|key| current[*key] == original[*key]),
            "execution changed while waiting for input"
        );
        let same_round =
            current["round"] == original["round"] && current["attempt"] == original["attempt"];
        let replay = replay_index(&current, &self.action, nudge.as_deref()).filter(|index| {
            let event = &current["audit"][*index];
            same_round
                || (operation == "continue"
                    && event["fromRound"] == original["round"]
                    && event["fromAttempt"] == original["attempt"])
        });
        if let Some(index) = replay {
            let mut event = current["audit"][index].clone();
            let inbox = messages::Inbox::open(&state)?;
            authorized_generation(&state, scope, token.as_deref(), operation)?;
            let (id, evidence) = inbox
                .worker_input_receipt(&current, &event)?
                .context("input is pending or uncertain; verify the prior input before retrying")?;
            inbox
                .sync_directory()
                .context("sync the worker input receipt")?;
            if event["result"] == "pending" {
                event["message"] = json!(id);
                event["inputEvidence"] = evidence;
                event["result"] = json!("accepted");
                current["audit"][index] = event.clone();
                save(&path, &dir, &current)?;
            } else {
                messages::sync_directory(&dir).context("sync the settled worker input")?;
            }
            return Ok(input_response(self.dispatch, &event, true));
        }
        ensure!(
            !original_unresolved && !assignment::input_unresolved(&current),
            "input is pending or uncertain; verify the prior input before retrying"
        );
        ensure!(same_round, "assignment changed while waiting for input");
        ensure!(
            current["controllerOwner"].is_null()
                && assignment::identified(&current)
                && current["executionId"].is_string()
                && current["tmuxSocket"].as_str().is_some_and(|socket| Path::new(socket).is_absolute())
                && current["create"]["result"] == "accepted"
                && current["create"]["receipt"].is_string(),
            "input requires a confirmed native launch; verify the original execution before retrying"
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
                && observed["executionId"] == current["executionId"]
                && observed["receipt"] == current["create"]["receipt"]
                && worker["state"] == "running",
            "execution is not a verified writable worker"
        );
        let legacy_callback = operation == "continue"
            && current[controller::CALLBACK_HASH].is_null()
            && current[controller::CALLBACK_REF_HASH].is_null();
        if legacy_callback {
            backend::verify_legacy_worker_callback(&state, self.dispatch, &observed)?;
        }

        let (mut next, index, text, generation) = {
            let _inbox = messages::lock_for_publish(&state)?;
            let generation = authorized_generation(&state, scope, token.as_deref(), operation)?;
            let history =
                view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
            let mut projected = current.clone();
            view::project(&mut projected, &history, Some(true), false)?;
            let mut next = current.clone();
            let mut event = json!({"id":format!("i-{}", Uuid::new_v4().simple()),
                "operation":operation, "result":"pending", "ownerGeneration":generation,
                "fromRound":current["round"], "fromAttempt":current["attempt"]});
            let text = match self.action {
                Action::Continue { task, report } => {
                    ensure!(
                        projected["state"] == "done",
                        "dispatch has not done its current task yet"
                    );
                    let (task, report) = next_paths(&current, &projected, &history, task, report)?;
                    ensure!(
                        legacy_callback || current[controller::CALLBACK_HASH]
                            .as_str()
                            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
                        "original worker has no independent callback credential; verify its launch before continuing"
                    );
                    let round = current["round"]
                        .as_u64()
                        .and_then(|value| value.checked_add(1))
                        .context("invalid next round")?;
                    let attempt = format!("a-{}", Uuid::new_v4().simple());
                    let reference = (!legacy_callback)
                        .then(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
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
                    if let Some(reference) = &reference {
                        next[controller::CALLBACK_REF_HASH] = json!(messages::body_hash(reference));
                    } else {
                        event["legacyOwnerCallback"] = json!(true);
                    }
                    event["task"] = json!(task);
                    event["report"] = json!(report);
                    continued_brief(
                        self.dispatch,
                        &task,
                        &report,
                        &attempt,
                        reference.as_deref(),
                    )
                }
                Action::Nudge(_) => {
                    ensure!(
                        projected["state"] != "done",
                        "dispatch already finished its task; give the next one with continue"
                    );
                    ensure!(
                        matches!(projected["state"].as_str(), Some("working" | "asking")),
                        "current round is not writable"
                    );
                    let text = nudge.as_deref().context("missing nudge text")?;
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
            (next, index, text, generation)
        };

        let mut first_write_error = None;
        let answer = backend::submit_checked(&state, &current, &text, &mut |boundary| {
            use crate::tmux::InputBoundary::{Enter, PrepareEnter, PrepareText, Text};
            let check = || -> Result<()> {
                if matches!(boundary, PrepareText | PrepareEnter) {
                    let _inbox = messages::lock_for_publish(&state)?;
                    ensure!(
                        super::read_record(&path)? == next,
                        "assignment changed before input"
                    );
                    let history =
                        view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
                    let mut projected = current.clone();
                    view::project(&mut projected, &history, Some(true), false)?;
                    if operation == "nudge" {
                        ensure!(
                            matches!(projected["state"].as_str(), Some("working" | "asking")),
                            "current round is no longer writable; use continue after completion"
                        );
                    } else {
                        ensure!(
                            projected["state"] == "done",
                            "continue lost its completed base round"
                        );
                    }
                } else {
                    ensure!(
                        authorized_generation(&state, scope, token.as_deref(), operation)?
                            == generation,
                        "scope owner changed before input"
                    );
                }
                Ok(())
            };
            check().map_err(|error| {
                let diagnostic = format!("{error:#}");
                let result = match boundary {
                    PrepareText | Text => {
                        first_write_error = Some(error);
                        "definitely-not-sent"
                    }
                    PrepareEnter | Enter => "uncertain",
                };
                json!({"result":result, "diagnostic":diagnostic})
            })
        })
        .unwrap_or_else(|error| json!({"result":"uncertain","diagnostic":format!("{error:#}")}));
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
        let mut inbox = messages::Inbox::open(&state)?;
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
        if let Some(evidence) = answer.get("inputEvidence") {
            event["inputEvidence"] = evidence.clone();
        }
        if outcome == "accepted" {
            event["message"] = json!(inbox.publish_worker_input(&next, &event)?);
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
        if let Some(error) = first_write_error {
            return Err(error);
        }
        if outcome != "accepted" {
            bail!("{operation} {outcome}: {}", event["diagnostic"]);
        }
        Ok(input_response(self.dispatch, &event, false))
    }
}

fn input_response(dispatch: &str, event: &Value, duplicate: bool) -> Value {
    let mut result = json!({"dispatch":dispatch,"operation":event["operation"],"result":"accepted",
        "round":event["round"],"attempt":event["attempt"],"audit":event["id"],"message":event["message"]});
    let action = if event["operation"] == "continue" {
        "continued"
    } else {
        "nudged"
    };
    result[action] = json!(dispatch);
    if duplicate {
        result["duplicate"] = json!(true);
    }
    result
}

fn replay_index(row: &Value, action: &Action<'_>, nudge: Option<&str>) -> Option<usize> {
    let audit = row["audit"].as_array()?;
    let unresolved = assignment::input_unresolved(row);
    audit.iter().rposition(|event| {
        event["operation"] == action.name()
            && matches!(event["result"].as_str(), Some("accepted" | "pending"))
            && (!unresolved || event["result"] == "pending")
            && event["round"] == row["round"]
            && event["attempt"] == row["attempt"]
            && match action {
                Action::Continue { task, report } => {
                    let resolved = task.canonicalize().or_else(|error| {
                        if error.kind() == ErrorKind::NotFound {
                            canonical_parent_path(task)
                        } else {
                            Err(error)
                        }
                    });
                    resolved
                        .ok()
                        .and_then(|path| path_text(path).ok())
                        .is_some_and(|task| event["task"] == task)
                        && canonical_parent_path(report)
                            .ok()
                            .and_then(|path| path_text(path).ok())
                            .is_some_and(|report| event["report"] == report)
                }
                Action::Nudge(_) => event["text"].as_str().map(str::trim_end) == nudge,
            }
    })
}

fn nudge_text(text: &str) -> Result<String> {
    let text = text
        .replace("\r\n", "\n")
        .split_terminator([
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ])
        .collect::<Vec<_>>()
        .join(" ");
    ensure!(
        !text.trim_end().is_empty() && !text.chars().any(char::is_control),
        "invalid nudge text"
    );
    Ok(text.trim_end().to_owned())
}

fn authorized_generation(
    state: &Path,
    scope: &str,
    token: Option<&str>,
    operation: &str,
) -> Result<Value> {
    let owners = controller::Owners::read(state)?;
    owners.require(scope, token, operation)?;
    Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
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
    let (task, report) = input_paths(current, task, report)?;
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
    Ok((task, report))
}

fn input_paths(current: &Value, task: &Path, report: &Path) -> Result<(String, String)> {
    let task = task.canonicalize().context("resolve task file")?;
    ensure!(task.is_file(), "--task must name a file");
    let original = Path::new(
        current["report"]
            .as_str()
            .context("missing original report")?,
    );
    let report = canonical_parent_path(report)?;
    ensure!(
        original.parent() == report.parent(),
        "next report must be in the original report directory"
    );
    Ok((path_text(task)?, path_text(report)?))
}

fn canonical_parent_path(path: &Path) -> std::io::Result<PathBuf> {
    let missing = || std::io::Error::new(ErrorKind::InvalidInput, "missing parent or filename");
    let absolute = std::path::absolute(path)?;
    let parent = absolute.parent().ok_or_else(missing)?.canonicalize()?;
    Ok(parent.join(absolute.file_name().ok_or_else(missing)?))
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
    reference: Option<&str>,
) -> String {
    let task = task.replace('\'', "'\\''");
    let report = report.replace('\'', "'\\''");
    let reference = reference.map_or_else(String::new, |reference| {
        format!(" --callback-ref {reference}")
    });
    format!("[router] New task for you (dispatch {dispatch}). Read and do the task in '{task}'. \
        Stop any server or background process you started before reporting. Write the report to '{report}'. \
        Run \"$WFC_BIN\" done --report '{report}' --attempt {attempt}{reference}. \
        If you need a decision, run \"$WFC_BIN\" ask \"<question>\" --attempt {attempt}{reference} and end your turn.")
}
