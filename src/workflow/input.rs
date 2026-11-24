//! Public input to the existing worker, with round changes committed before typing.
mod recovery;

use super::{
    assignment, backend, controller, ensure_state_writable, herdr, messages, record_scope, view,
    writable_state_dir, write_json,
};
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
    "wfc continue DISPATCH --task FILE --report FILE [--owner-token TOKEN] [--no-send-proof PROOF]";
pub const NUDGE_USAGE: &str =
    "wfc nudge DISPATCH --text TEXT [--message-id KEY] [--owner-token TOKEN] [--no-send-proof PROOF]";
pub(super) const NO_SEND_RECOVERY: &str = "herdrNoSendRecovery";

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
    key: Option<&'a str>,
    no_send_proof: Option<&'a str>,
    action: Action<'a>,
}

pub(super) trait HerdrInputTarget {
    fn bind_continued_execution(&self, next: &mut Value);
    fn save_writer_evidence(&self, next: &mut Value);
    fn submit(
        &mut self,
        action: wfc_herdr::CodexInput,
        text: &str,
        check: impl FnMut(wfc_herdr::InputBoundary) -> std::io::Result<()>,
    ) -> Value;
}

impl<'a> Request<'a> {
    pub fn parse(name: &str, args: &'a [OsString]) -> Result<Self> {
        let mut dispatch = None;
        let (token, key, no_send_proof, action) = if name == "continue" {
            let [token, task, report, no_send_proof] = messages::options(
                args,
                ["--owner-token", "--task", "--report", "--no-send-proof"],
                Some((&mut dispatch, "dispatch ID")),
            )?;
            (
                token,
                None,
                no_send_proof,
                Action::Continue {
                    task: Path::new(task.context("missing --task")?),
                    report: Path::new(report.context("missing --report")?),
                },
            )
        } else {
            let [token, text, key, no_send_proof] = messages::options(
                args,
                ["--owner-token", "--text", "--message-id", "--no-send-proof"],
                Some((&mut dispatch, "dispatch ID")),
            )?;
            ensure!(
                key.is_none_or(|key| !key.trim().is_empty() && !key.chars().any(char::is_control)),
                "invalid --message-id"
            );
            (
                token,
                key,
                no_send_proof,
                Action::Nudge(text.context("missing --text")?),
            )
        };
        ensure!(
            no_send_proof.is_none_or(
                |proof| proof.len() == 64 && proof.bytes().all(|byte| byte.is_ascii_hexdigit())
            ),
            "invalid --no-send-proof"
        );
        let dispatch = dispatch
            .filter(|id| super::valid_id(id))
            .context("invalid or missing dispatch ID")?;
        Ok(Self {
            dispatch,
            token,
            key,
            no_send_proof,
            action,
        })
    }

    pub fn run(self) -> Result<Value> {
        self.run_in(
            writable_state_dir()?,
            herdr::InputTarget::verify,
            ensure_state_writable,
            messages::Inbox::open,
        )
    }

    fn run_in<T: HerdrInputTarget>(
        self,
        state: PathBuf,
        verify_herdr: impl FnOnce(&Path, &Value) -> Result<T>,
        check_write: impl Fn(&Path) -> Result<()>,
        open_settlement: impl FnOnce(&Path) -> Result<messages::Inbox>,
    ) -> Result<Value> {
        let operation = self.action.name();
        let dir = state.join("dispatches");
        let path = dir.join(format!("{}.json", self.dispatch));
        let original = assignment::public_record(&dir, self.dispatch)?
            .with_context(|| format!("unknown dispatch {}", self.dispatch))?;
        ensure!(
            original["legacyArchived"] != true,
            "offline legacy assignment has no input target"
        );
        let herdr_input = original["backend"] == "herdr";
        let require_host = |row: &Value, replay: Option<usize>| -> Result<()> {
            if row["backend"] == "herdr" {
                ensure!(
                    row["host"] == "codex"
                        || replay.is_some_and(|index| row["audit"][index]["result"] == "accepted"),
                    "Herdr input requires a Codex assignment"
                );
                return Ok(());
            }
            ensure!(
                row["backend"] == "tmux"
                    && (row["host"] == "codex"
                        || replay.is_some_and(|index| row["audit"][index]["result"] == "accepted")),
                "{operation} requires a supported live Codex/tmux assignment"
            );
            Ok(())
        };
        let scope = record_scope(&original).context("invalid dispatch scope")?;
        let token = controller::token(self.token);
        controller::Owners::read(&state)?.require(scope, token.as_deref(), operation)?;
        let nudge = match &self.action {
            Action::Nudge(text) => Some(nudge_text(text)?),
            _ => None,
        };
        let original_unresolved = assignment::input_unresolved(&original);
        let initial_replay = replay_index(&original, &self.action, nudge.as_deref(), self.key)?;
        require_host(&original, initial_replay)?;
        ensure!(
            !original_unresolved || initial_replay.is_some() || self.no_send_proof.is_some(),
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
            ["host", "backend", "create"]
                .iter()
                .all(|key| current[*key] == original[*key])
                && if herdr_input {
                    herdr::same_binding(&original, &current)
                } else {
                    current["executionId"] == original["executionId"]
                        && current["tmuxSocket"] == original["tmuxSocket"]
                },
            "execution changed while waiting for input"
        );
        let same_round =
            current["round"] == original["round"] && current["attempt"] == original["attempt"];
        let replay =
            replay_index(&current, &self.action, nudge.as_deref(), self.key)?.filter(|index| {
                let event = &current["audit"][*index];
                same_round
                    || (operation == "nudge" && event["result"] == "accepted")
                    || (operation == "continue"
                        && event["fromRound"] == original["round"]
                        && event["fromAttempt"] == original["attempt"])
            });
        require_host(&current, replay)?;
        ensure!(
            assignment::unresolved_handoff(&current).is_none()
                || replay.is_some_and(|index| current["audit"][index]["result"] == "accepted"),
            "handoff is unresolved; reconcile the original execution before input"
        );
        if let Some(proof) = self.no_send_proof {
            ensure!(
                herdr_input && same_round,
                "zero-send recovery requires the original Herdr input"
            );
            let _inbox = messages::Inbox::open(&state)?;
            authorized_generation(&state, scope, token.as_deref(), operation)?;
            let (mut restored, mut event) = recovery::restore(current.clone(), proof)?;
            ensure!(
                event["operation"] == operation
                    && match &self.action {
                        Action::Nudge(_) =>
                            event["requestKey"] == json!(self.key)
                                && event["text"].as_str() == nudge.as_deref(),
                        Action::Continue { task, report } =>
                            same_continue_paths(&event, task, report),
                    },
                "zero-send proof requires the original input request"
            );
            ensure!(
                herdr::same_binding(&current, &restored),
                "zero-send proof does not match the original Herdr execution"
            );
            let message_key = messages::Inbox::worker_input_key(&event)?;
            let history =
                view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
            ensure!(
                !history.iter().any(|message| {
                    message["messageId"] == message_key
                        || (operation == "continue"
                            && message["round"] == event["round"]
                            && message["attempt"] == event["attempt"])
                }),
                "worker history conflicts with zero-send proof; keep the input pending"
            );
            event["result"] = json!("definitely-not-sent");
            event["inputEvidence"] = json!("herdr-first-write-blocked");
            event["diagnostic"] = json!("Herdr input was blocked before its first write");
            *restored["audit"]
                .as_array_mut()
                .and_then(|events| events.last_mut())
                .context("missing pending input audit")? = event.clone();
            authorized_generation(&state, scope, token.as_deref(), operation)?;
            save(&state, &path, &restored)?;
            return Ok(
                json!({"dispatch":self.dispatch,"operation":operation,"result":"definitely-not-sent",
                "round":restored["round"],"attempt":restored["attempt"],"audit":event["id"]}),
            );
        }
        if let Some(index) = replay {
            let mut event = current["audit"][index].clone();
            let mut inbox = messages::Inbox::open(&state)?;
            authorized_generation(&state, scope, token.as_deref(), operation)?;
            let mut receipt = inbox.worker_input_receipt(&current, &event)?;
            if receipt.is_none()
                && operation == "continue"
                && matches!(event["result"].as_str(), Some("pending" | "uncertain"))
                && inbox.verified_continued_input_callback(&current, &event)?
            {
                let evidence = json!(messages::CONTINUED_INPUT_CALLBACK);
                event["inputEvidence"] = evidence.clone();
                receipt = Some((inbox.publish_worker_input(&current, &event)?, evidence));
            }
            let (id, evidence) = receipt
                .context("input is pending or uncertain; verify the prior input before retrying")?;
            if event["result"] == "accepted" && assignment::input_unresolved(&current) {
                return Ok(input_response(self.dispatch, &event, true));
            }
            inbox
                .sync_directory()
                .context("sync the worker input receipt")?;
            if matches!(event["result"].as_str(), Some("pending" | "uncertain")) {
                event["message"] = json!(id);
                event["inputEvidence"] = evidence;
                event["result"] = json!("accepted");
                event
                    .as_object_mut()
                    .context("invalid input audit")?
                    .remove("diagnostic");
                current["audit"][index] = event.clone();
                current
                    .as_object_mut()
                    .context("invalid input assignment")?
                    .remove(NO_SEND_RECOVERY);
                save(&state, &path, &current)?;
            } else {
                messages::sync_directory(&dir).context("sync the settled worker input")?;
            }
            return Ok(input_response(self.dispatch, &event, true));
        }
        // Herdr's pane lock serializes transports. The attempt lock also
        // serializes a fresh reminder with `done`, which can publish during status.
        let _completion = (herdr_input && operation == "nudge")
            .then(|| assignment::close_lock(&state, &current))
            .transpose()?;
        ensure!(
            !original_unresolved && !assignment::input_unresolved(&current),
            "input is pending or uncertain; verify the prior input before retrying"
        );
        ensure!(same_round, "assignment changed while waiting for input");
        ensure!(
            assignment::unresolved_resume(&current).is_none(),
            "resume is unresolved; verify the prior input before retrying"
        );
        ensure!(
            current["controllerOwner"].is_null()
                && assignment::identified(&current)
                && (herdr_input
                    || (current["executionId"].is_string()
                        && current["tmuxSocket"]
                            .as_str()
                            .is_some_and(|socket| Path::new(socket).is_absolute())))
                && current["create"]["result"] == "accepted"
                && current["create"]["receipt"].is_string(),
            "input requires a confirmed native launch; verify the original execution before retrying"
        );
        ensure!(
            backend::archive(&state, &current)?.is_none(),
            "execution resources are closing or closed; use resume for an exited worker"
        );
        let mut herdr_target = herdr_input
            .then(|| verify_herdr(&state, &current))
            .transpose()?;
        let legacy_callback = operation == "continue"
            && !herdr_input
            && current[controller::CALLBACK_HASH].is_null()
            && current[controller::CALLBACK_REF_HASH].is_null();
        let worker = if herdr_target.is_some() {
            json!({"state":"running"})
        } else {
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
            if legacy_callback {
                backend::verify_legacy_worker_callback(&state, self.dispatch, &observed)?;
            }
            worker
        };

        // The scope lock prevents this scope's owner from changing across
        // observation and send. Recheck its clock at the first socket write.
        let herdr_owners = herdr_input
            .then(|| controller::Owners::read(&state))
            .transpose()?;
        // Only this process knows the random proof until it witnesses a refused first write.
        let staged_proof =
            herdr_input.then(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
        let (mut next, index, text, generation) = {
            let inbox = messages::Inbox::open(&state)?;
            let generation = authorized_generation(&state, scope, token.as_deref(), operation)?;
            let history =
                view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
            ensure!(
                !inbox.unresolved_reply(&current, None)?
                    && (!herdr_input || !herdr::unsettled_reply(&current, &history)),
                "reply is pending or uncertain on this execution; verify it before input"
            );
            let mut projected = current.clone();
            projected["worker"] = worker.clone();
            view::project(&mut projected, &history, Some(true), false)?;
            let mut next = current.clone();
            if let Some(target) = &herdr_target {
                target.save_writer_evidence(&mut next);
            }
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
                        legacy_callback
                            || current[controller::CALLBACK_HASH]
                                .as_str()
                                .is_some_and(|hash| hash.len() == 64
                                    && hash.bytes().all(|b| b.is_ascii_hexdigit())),
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
                    if let Some(target) = &herdr_target {
                        target.bind_continued_execution(&mut next);
                    }
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
                    if let Some(key) = self.key {
                        event["requestKey"] = json!(key);
                    }
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
            if let Some(proof) = &staged_proof {
                recovery::stage(&current, &mut next, proof, index)?;
            }
            save(&state, &path, &next)?;
            (next, index, text, generation)
        };

        let mut first_write_error = None;
        use crate::tmux::InputBoundary::{Enter, PrepareEnter, PrepareText, Text};
        let mut check_boundary = |boundary| {
            let check = || -> Result<()> {
                if matches!(boundary, PrepareText | PrepareEnter) {
                    let inbox = messages::Inbox::open(&state)?;
                    ensure!(
                        super::read_record(&path)? == next,
                        "assignment changed before input"
                    );
                    let history =
                        view::merged_history(&current, messages::history(&state, self.dispatch)?)?;
                    ensure!(
                        !inbox.unresolved_reply(&current, None)?
                            && (!herdr_input || !herdr::unsettled_reply(&current, &history)),
                        "reply is pending or uncertain on this execution; verify it before input"
                    );
                    let mut projected = current.clone();
                    projected["worker"] = worker.clone();
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
                    // Herdr calls this at the first socket write, after its final pane checks.
                    check_input_at_write(
                        || check_write(&state),
                        || match &herdr_owners {
                            Some(owners) => {
                                owner_generation(owners, scope, token.as_deref(), operation)
                            }
                            None => {
                                authorized_generation(&state, scope, token.as_deref(), operation)
                            }
                        },
                        &generation,
                    )?;
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
        };
        let answer = if let Some(target) = &mut herdr_target {
            let action = if operation == "continue" {
                wfc_herdr::CodexInput::Continue
            } else {
                wfc_herdr::CodexInput::Nudge
            };
            target.submit(action, &text, |boundary| {
                let boundary = match boundary {
                    wfc_herdr::InputBoundary::Prepare => PrepareText,
                    wfc_herdr::InputBoundary::Send => Text,
                };
                check_boundary(boundary).map_err(|error| std::io::Error::other(error.to_string()))
            })
        } else {
            backend::submit_checked(&state, &current, &text, true, &mut check_boundary)
                .unwrap_or_else(
                    |error| json!({"result":"uncertain","diagnostic":format!("{error:#}")}),
                )
        };
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
        // Only the local preparation and first-write callbacks set this error.
        // An RPC response can report zero enqueue after request bytes were sent.
        let rejected_before_write = first_write_error.is_some();
        let audit_id = next["audit"][index]["id"].clone();
        let settlement_error = |error, outcome| {
            let proof = recovery::proof_after_refusal(
                staged_proof.as_deref(),
                rejected_before_write,
                outcome,
            );
            recovery::settlement_error(error, proof, self.dispatch, &audit_id)
        };
        let mut inbox =
            open_settlement(&state).map_err(|error| settlement_error(error, outcome))?;
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
        if let Some(target) = &herdr_target {
            target.save_writer_evidence(&mut next);
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
        next.as_object_mut()
            .context("invalid input assignment")?
            .remove(NO_SEND_RECOVERY);
        save(&state, &path, &next).map_err(|error| settlement_error(error, outcome))?;
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

fn replay_index(
    row: &Value,
    action: &Action<'_>,
    nudge: Option<&str>,
    key: Option<&str>,
) -> Result<Option<usize>> {
    let Some(audit) = row["audit"].as_array() else {
        return Ok(None);
    };
    if matches!(action, Action::Nudge(_)) {
        let Some(key) = key else {
            return Ok(None);
        };
        let matches = |event: &Value| event["operation"] == "nudge" && event["requestKey"] == key;
        ensure!(
            audit
                .iter()
                .filter(|event| matches(event))
                .all(|event| event["text"].as_str() == nudge),
            "message request {key} conflicts with an existing reminder"
        );
        return Ok(audit.iter().rposition(|event| {
            matches(event)
                && (event["result"] == "accepted"
                    || (event["result"] == "pending"
                        && event["round"] == row["round"]
                        && event["attempt"] == row["attempt"]))
        }));
    }
    Ok(audit.iter().rposition(|event| {
        event["operation"] == action.name()
            && matches!(
                event["result"].as_str(),
                Some("pending" | "uncertain" | "accepted")
            )
            && event["round"] == row["round"]
            && event["attempt"] == row["attempt"]
            && match action {
                Action::Continue { task, report } => same_continue_paths(event, task, report),
                Action::Nudge(_) => false,
            }
    }))
}

fn same_continue_paths(event: &Value, task: &Path, report: &Path) -> bool {
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
    let text = text.trim_end();
    ensure!(
        !text.ends_with(['↑', '↓']),
        "a trailing scroll arrow cannot be verified before sending"
    );
    Ok(text.to_owned())
}

fn authorized_generation(
    state: &Path,
    scope: &str,
    token: Option<&str>,
    operation: &str,
) -> Result<Value> {
    owner_generation(&controller::Owners::read(state)?, scope, token, operation)
}

fn owner_generation(
    owners: &controller::Owners,
    scope: &str,
    token: Option<&str>,
    operation: &str,
) -> Result<Value> {
    Ok(json!(owners.authorized_generation(scope, token, operation)?))
}

fn check_input_at_write(
    check_writable: impl FnOnce() -> Result<()>,
    current_generation: impl FnOnce() -> Result<Value>,
    expected: &Value,
) -> Result<()> {
    check_writable()?;
    ensure!(
        &current_generation()? == expected,
        "scope owner changed before input"
    );
    Ok(())
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
            let execution = if row["backend"] == "herdr" {
                herdr::try_input_lock(state, row)?
            } else {
                assignment::try_reply_lock(state, row)?
            };
            if let Some(execution) = execution {
                return Ok((scope_lock, execution));
            }
        }
        let remaining = Duration::from_secs(30).saturating_sub(start.elapsed());
        ensure!(!remaining.is_zero(), "input_lock_timeout");
        sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn save(state: &Path, path: &Path, row: &Value) -> Result<()> {
    ensure_state_writable(state)?;
    write_json(path, row, None)?;
    messages::sync_directory(&state.join("dispatches"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use wfc_herdr::{CodexInput, InputBoundary};

    #[test]
    fn write_fence_precedes_final_owner_check() -> Result<()> {
        let order = RefCell::new(Vec::new());
        let generation = json!(3);
        check_input_at_write(
            || {
                order.borrow_mut().push("write protection");
                Ok(())
            },
            || {
                order.borrow_mut().push("lease and generation");
                Ok(generation.clone())
            },
            &generation,
        )?;
        assert_eq!(
            *order.borrow(),
            ["write protection", "lease and generation"]
        );

        let denied = check_input_at_write(
            || bail!("legacy state write blocked"),
            || panic!("a closed write fence must stop the send callback"),
            &generation,
        );
        assert_eq!(
            denied.unwrap_err().to_string(),
            "legacy state write blocked"
        );
        Ok(())
    }

    #[test]
    fn herdr_rejected_first_write_delivers_proof_and_recovers_staged_input() -> Result<()> {
        struct State(PathBuf);
        impl Drop for State {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        struct RefusedInput;
        impl HerdrInputTarget for RefusedInput {
            fn bind_continued_execution(&self, _: &mut Value) {
                panic!("fixture stages a reminder");
            }

            fn save_writer_evidence(&self, _: &mut Value) {}

            fn submit(
                &mut self,
                action: CodexInput,
                text: &str,
                mut check: impl FnMut(InputBoundary) -> std::io::Result<()>,
            ) -> Value {
                assert!(matches!(action, CodexInput::Nudge));
                assert_eq!(
                    text,
                    "[router] Message from the controller: ordinary reminder"
                );
                check(InputBoundary::Prepare).expect("fixture passes ordinary preparation");
                assert!(check(InputBoundary::Send).is_err());
                json!({"result":"definitely-not-sent","diagnostic":"fixture first write refused"})
            }
        }

        let state = State(std::env::temp_dir().join(format!("wfc-no-send-{}", Uuid::new_v4())));
        let dispatch = "d-12345678";
        let attempt = "a-123456789abc";
        let dir = state.0.join("dispatches");
        fs::create_dir_all(&dir)?;
        File::create(dir.join(format!("{dispatch}.herdr-lock")))?;
        let path = dir.join(format!("{dispatch}.json"));
        let receipt = state.0.join("launches/fixture.json");
        let endpoint = state.0.join("unused.sock");
        let original = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"role":"worker",
            "host":"codex","backend":"herdr","executionId":attempt,"endpoint":endpoint,
            "create":{"result":"accepted","receipt":receipt},"pendingInput":false,"audit":[],
            "backendHandle":{"dispatch":dispatch,"attempt":attempt,"round":1,"endpoint":endpoint,
                "receipt":receipt,"pane":"fixture-pane","writerProcesses":[]}});
        save(&state.0, &path, &original)?;

        let mut args: Vec<OsString> = [
            dispatch,
            "--text",
            "ordinary reminder",
            "--message-id",
            "original",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let denied = Cell::new(false);
        let error = Request::parse("nudge", &args)?
            .run_in(
                state.0.clone(),
                |_, _| Ok(RefusedInput),
                |_| {
                    let staged = super::super::read_record(&path)?;
                    assert_eq!(staged["audit"][0]["result"], "pending");
                    assert!(staged[NO_SEND_RECOVERY].is_object());
                    denied.set(true);
                    bail!("fixture refused the first write")
                },
                |_| {
                    assert!(denied.get());
                    bail!("fixture settlement unavailable")
                },
            )
            .expect_err("a refused first write must return a settlement error");
        let issued: Value = serde_json::from_str(
            &error
                .downcast_ref::<controller::ControllerError>()
                .context("missing structured no-send response")?
                .to_string(),
        )?;
        assert_eq!(issued["error"], "herdr_no_send_recovery_required");
        let proof = issued["noSendProof"]
            .as_str()
            .context("no proof was delivered")?;
        let staged = super::super::read_record(&path)?;
        assert_eq!(staged["audit"][0]["id"], issued["audit"]);
        assert!(!staged.to_string().contains(proof));
        assert!(!state.0.join("messages.json").exists());

        args.extend([OsString::from("--no-send-proof"), OsString::from(proof)]);
        let settled = Request::parse("nudge", &args)?.run_in::<RefusedInput>(
            state.0.clone(),
            |_, _| panic!("recovery must not open a transport"),
            |_| panic!("recovery must not authorize another write"),
            |_| panic!("recovery must not enter transport settlement"),
        )?;
        assert_eq!(settled["audit"], issued["audit"]);
        assert_eq!(settled["result"], "definitely-not-sent");
        assert_eq!(settled["attempt"], attempt);
        let final_record = super::super::read_record(&path)?;
        assert_eq!(final_record["audit"][0]["id"], settled["audit"]);
        assert_eq!(final_record["audit"][0]["result"], "definitely-not-sent");
        assert!(final_record.get(NO_SEND_RECOVERY).is_none());
        assert!(!state.0.join("messages.json").exists());
        Ok(())
    }
}
