//! A durable question claim fences duplicate input, including after a lost reply.
use super::{
    literal_question_argument, state_dir, valid_id, Assignment, Inbox, InputResult, Message,
    ReplyRecord, State,
};
use crate::workflow::{assignment, backend, herdr};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{ffi::OsString, path::Path};

pub const REPLY_USAGE: &str = "wfc reply MESSAGE ANSWER [--owner-token TOKEN]";

pub struct Reply<'a> {
    question: u64,
    answer: &'a str,
}

impl<'a> Reply<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut values = Vec::new();
        let mut args = args.iter();
        let mut options = true;
        let mut token_seen = false;
        while let Some(arg) = args.next() {
            let arg = arg.to_str().context("expected UTF-8 argument")?;
            if options && arg == "--" {
                options = false;
            } else if options && (arg == "--owner-token" || arg.starts_with("--owner-token=")) {
                ensure!(!token_seen, "duplicate --owner-token");
                token_seen = true;
                if arg == "--owner-token" {
                    args.next()
                        .and_then(|value| value.to_str())
                        .filter(|value| !value.starts_with("--"))
                        .context("--owner-token requires a value")?;
                }
                // AUTH: this baseline has only unowned assignments. The
                // ownership check belongs in validate, on both passes.
            } else {
                ensure!(
                    !options || literal_question_argument(arg, &["--owner-token"]),
                    "unknown reply option"
                );
                values.push(arg);
            }
        }
        let [question, answer] = values.as_slice() else {
            bail!("expected a numeric question id and one answer");
        };
        Ok(Self {
            question: question.parse().context("expected a numeric question id")?,
            answer,
        })
    }

    pub fn run(self) -> Result<Value> {
        let state = state_dir()?;
        let answer = self
            .answer
            .replace("\r\n", "\n")
            .split_terminator([
                '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}',
                '\u{2029}',
            ])
            .collect::<Vec<_>>()
            .join(" ");
        let question = self.question;
        let (worker, uid, row) = {
            let mut inbox = Inbox::open(&state)?;
            let original = inbox
                .messages
                .iter()
                .find(|m| m.id == question && m.question_payload.is_some())
                .with_context(|| format!("no question with id {question}"))?;
            let row = validate(&state, &inbox, original)?;
            ensure!(
                !inbox.messages.iter().any(|m| {
                    m.reply_payload.as_ref().is_some_and(|reply| {
                        reply.question_message_id == original.message_id
                            && reply.input_result.claims_question()
                    })
                }),
                "question {question} was already answered or has a reply pending confirmation"
            );
            let mut claim = Message::new(
                inbox.messages.len() as u64 + 1,
                original.worker.clone(),
                None,
            );
            claim.reply_payload = Some(ReplyRecord {
                to: question,
                question_message_id: original.message_id.clone(),
                answer: answer.clone(),
                input_result: InputResult::Pending,
                diagnostic: Some("reply reserved, submission has not started".into()),
                category: None,
            });
            let worker = original.worker.clone();
            let uid = claim.message_id.clone();
            inbox.publish(claim).context("reserve answer")?;
            (worker, uid, row)
        };
        let text = format!("[router] Answer to question {question}: {answer}");
        let mut started = false;
        let begin = || -> Result<()> {
            let mut inbox = Inbox::open(&state)?;
            let claim = reply_record(&mut inbox, &uid)?;
            ensure!(
                claim.input_result == InputResult::Pending,
                "reply is no longer pending"
            );
            let original_id = claim.question_message_id.clone();
            let original = inbox
                .messages
                .iter()
                .find(|m| {
                    m.id == question
                        && m.message_id == original_id
                        && m.worker == worker
                        && m.question_payload.is_some()
                })
                .context("question changed while waiting for input")?;
            let current = validate(&state, &inbox, original)?;
            ensure!(
                [
                    "host",
                    "backend",
                    "tmuxSocket",
                    "endpoint",
                    "executionId",
                    "legacyWorkerPane",
                    "legacyLaunchId"
                ]
                .iter()
                .all(|key| row[*key] == current[*key])
                    && row["create"]["receipt"] == current["create"]["receipt"]
                    && [
                        "pane",
                        "terminal",
                        "workspace",
                        "nonce",
                        "service",
                        "endpoint"
                    ]
                    .iter()
                    .all(|key| row["backendHandle"][*key] == current["backendHandle"][*key]),
                "assignment changed while waiting for input"
            );
            update(
                &mut inbox,
                &uid,
                InputResult::Uncertain,
                Some("submission started; no confirmation stored".into()),
                Some("input_uncertain".into()),
            )?;
            started = true;
            Ok(())
        };

        let result = submit(&state, &row, &text, begin);
        let (outcome, diagnostic, category) = match result {
            Ok(result) => {
                let outcome = match (result["result"].as_str(), result["inputResult"].as_str()) {
                    (Some("accepted"), _) => InputResult::Accepted,
                    (Some("refused"), _) => InputResult::Refused,
                    (Some("definitely-not-sent"), _) | (_, Some("definitely-not-sent")) => {
                        InputResult::DefinitelyNotSent
                    }
                    _ => InputResult::Uncertain,
                };
                let diagnostic = result["diagnostic"].as_str().map(str::to_owned);
                let category = result["category"].as_str().map(str::to_owned);
                (outcome, diagnostic, category)
            }
            Err(error) if started => (
                InputResult::Uncertain,
                Some(format!("{error:#}")),
                Some("input_uncertain".into()),
            ),
            Err(error) => (
                InputResult::NotSubmitted,
                Some(format!("{error:#}")),
                Some("input_not_delivered".into()),
            ),
        };
        let diagnostic = diagnostic.or_else(|| {
            (outcome != InputResult::Accepted).then(|| "backend did not confirm input".into())
        });
        let category = category.or_else(|| match outcome {
            InputResult::Uncertain => Some("input_uncertain".into()),
            InputResult::DefinitelyNotSent => Some("input_not_delivered".into()),
            _ => None,
        });
        let mut inbox = Inbox::open(&state)?;
        update(&mut inbox, &uid, outcome, diagnostic.clone(), category)
            .context("save answer outcome")?;
        if outcome != InputResult::Accepted {
            let label = json!(outcome);
            bail!(
                "answer to question {question} failed with {}: {}",
                label.as_str().unwrap(),
                diagnostic.unwrap_or_default()
            );
        }
        Ok(json!({"replied":question, "dispatch":worker.dispatch}))
    }
}

fn validate(state: &Path, inbox: &Inbox, question: &Message) -> Result<Value> {
    let worker = &question.worker;
    ensure!(valid_id(&worker.dispatch), "invalid question dispatch");
    let (assignment, row) = Assignment::read(state, &worker.dispatch)?;
    ensure!(
        worker.round == assignment.worker.round
            && worker.attempt == assignment.worker.attempt
            && worker.role == assignment.worker.role
            && worker.scope() == assignment.worker.scope(),
        "question {} belongs to an attempt that is no longer accepted",
        question.id
    );
    inbox.require_question_round(&assignment, &row, Some(&worker.attempt))?;
    ensure!(
        row["createPending"] != true
            && (!row["host"].is_string() || row["create"]["result"] == "accepted"),
        "question {} belongs to an unconfirmed execution",
        question.id
    );
    Ok(row)
}

fn submit(
    state: &Path,
    row: &Value,
    text: &str,
    begin: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    // Both backends hold the input lock through submission. The scope owner
    // fence can be taken under that lock, before begin rechecks the assignment.
    match row["backend"].as_str() {
        Some("tmux") => {
            let _input_lock = assignment::reply_lock(state, row)?;
            ensure!(
                backend::archive(state, row)?.is_none(),
                "execution resources are closing or closed"
            );
            begin()?;
            backend::submit(state, row, text)
        }
        Some("herdr") => herdr::submit_reply(state, row, text, begin),
        _ => bail!("dispatch has no supported worker input backend"),
    }
}

fn reply_record<'a>(inbox: &'a mut Inbox, uid: &str) -> Result<&'a mut ReplyRecord> {
    inbox
        .messages
        .iter_mut()
        .find(|m| m.message_id == uid)
        .and_then(|m| m.reply_payload.as_mut())
        .context("reply claim is missing")
}

fn update(
    inbox: &mut Inbox,
    uid: &str,
    result: InputResult,
    diagnostic: Option<String>,
    category: Option<String>,
) -> Result<()> {
    let claim = inbox
        .messages
        .iter_mut()
        .find(|m| m.message_id == uid)
        .context("reply claim is missing")?;
    let reply = claim
        .reply_payload
        .as_mut()
        .context("invalid reply claim")?;
    reply.input_result = result;
    reply.diagnostic = diagnostic;
    reply.category = category;
    claim.delivery.advance(match result {
        InputResult::Accepted => State::Received,
        InputResult::Uncertain => State::Unknown,
        _ => State::Unsent,
    })?;
    claim.body_hash = Some(claim.computed_hash());
    inbox.save()
}
