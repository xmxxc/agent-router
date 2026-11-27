//! Publish live observations through the ordinary Inbox transaction.
use super::{recovery, Assignment, Inbox, Message};
use crate::workflow::{
    runtime_fault::{last_accepted_input, Notice, Observation},
    view,
};
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::{
    fs::{self, ReadDir},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Watch {
    entries: Option<ReadDir>,
}

pub(super) struct Sample {
    stamps: recovery::GenerationStamps,
    assignment: Assignment,
    row: Value,
    observed: Observation,
}

impl Watch {
    pub fn poll(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
        allowance: Duration,
    ) -> Vec<Sample> {
        let dir = state.join("dispatches");
        let read = |id: &str| {
            let generations = recovery::Generations::read(&dir, id);
            let row = generations.published()?;
            if row["dispatch"] != id
                || !row["scope"].as_str().is_some_and(|bound| {
                    !bound.is_empty() && scope.is_none_or(|scope| bound == scope)
                })
            {
                return None;
            }
            let observed = Observation::read(state, row)?;
            Some(Sample {
                stamps: generations.stamps(),
                assignment: serde_json::from_value(row.clone()).ok()?,
                row: row.clone(),
                observed,
            })
        };
        if allowance.is_zero() {
            return Vec::new();
        }
        if let Some(id) = dispatch {
            return read(id).into_iter().collect();
        }
        let start = Instant::now();
        if self.entries.is_none() {
            self.entries = fs::read_dir(&dir).ok();
        }
        let mut observed = Vec::new();
        for _ in 0..16 {
            if start.elapsed() >= allowance.min(recovery::SLICE) {
                break;
            }
            let Some(entry) = self.entries.as_mut().and_then(Iterator::next) else {
                self.entries = None;
                break;
            };
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if let Some(id) = recovery::dispatch_filename(name.as_encoded_bytes()) {
                observed.extend(read(id));
            }
        }
        observed
    }
}

impl Inbox {
    pub(super) fn publish_runtime(
        &mut self,
        samples: Vec<Sample>,
        max: Duration,
        stall: Duration,
    ) -> Result<()> {
        let dir = self.dir.join("dispatches");
        let mut changed = false;
        for sample in samples {
            let worker = &sample.assignment.worker;
            let observed = &sample.observed;
            if !sample.stamps.current(&dir, &worker.dispatch)
                || !observed.current(&self.dir, &sample.row)
            {
                continue;
            }
            if worker.require_message_access(&self.dir).is_err() {
                continue;
            }
            let current: Vec<_> = self
                .messages
                .iter()
                .filter(|m| {
                    m.worker.dispatch == worker.dispatch
                        && m.worker.round == worker.round
                        && m.worker.attempt == worker.attempt
                })
                .collect();
            if current.iter().any(|m| {
                m.is_done()
                    || m.additional_fields.get("type").and_then(Value::as_str) == Some("reclaimed")
            }) {
                continue;
            }

            let history: Vec<_> = current
                .iter()
                .filter_map(|message| {
                    matches!(
                        message.kind(),
                        super::MessageKind::Question
                            | super::MessageKind::Reply
                            | super::MessageKind::Nudge
                    )
                    .then(|| {
                        let mut view = message.view();
                        view["at"] = json!(message.created_at);
                        view
                    })
                })
                .collect();
            if view::awaiting_answer(&sample.row, &history) {
                continue;
            }
            let input_ms = last_accepted_input(&sample.row, &history);
            let notice = observed.notice(max, stall, input_ms);
            // An already reported host fault must not stop the clock.
            let deadline = notice
                .as_ref()
                .filter(|notice| notice.kind == "host_error")
                .and_then(|_| observed.deadline(max, input_ms));
            let activity = observed.activity();

            // Only a later event on the same rollout can recover an observed
            // host error or stall. The round deadline remains independent.
            let recovery = activity.and_then(|activity| {
                let last = current.iter().rev().find(|m| {
                    m.additional_fields
                        .get("executionGeneration")
                        .and_then(Value::as_str)
                        == Some(&observed.generation)
                        && matches!(
                            m.additional_fields.get("kind").and_then(Value::as_str),
                            Some("host_error" | "stall_timeout" | "runtime_recovered")
                        )
                })?;
                let fields = &last.additional_fields;
                let prior = fields.get("runtimeEvidence")?;
                let progressed = if fields["kind"] == "stall_timeout" {
                    activity["lastProgressAtMs"].as_u64()? > prior["lastProgressAtMs"].as_u64()?
                } else {
                    activity["offset"].as_u64()? > prior["offset"].as_u64()?
                        && activity["atMs"].as_u64()? >= prior["atMs"].as_u64()?
                        && activity["turn"] != prior["turn"]
                };
                (fields["kind"] != "runtime_recovered"
                    && activity["session"] == prior["session"]
                    && progressed)
                    .then(|| Notice::recovered(activity))
            });
            // Seal the observed physical and business identities into the
            // receipt key, keeping the entire history of keys across polls.
            for notice in recovery.into_iter().chain(notice).chain(deadline) {
                let key = format!(
                    "runtime:{}:{}:{}:{}:{}:{}:{}",
                    worker.dispatch,
                    worker.round,
                    worker.attempt,
                    observed.execution,
                    observed.generation,
                    notice.kind,
                    notice.fingerprint
                );
                if self.prior(&worker.dispatch, &key).is_some() {
                    continue;
                }
                let mut message = Message::new(self.next_id()?, worker.clone(), Some(key));
                message.report = sample.assignment.report.clone();
                message.additional_fields = Map::from_iter([
                    ("type".into(), json!(notice.message_type)),
                    ("kind".into(), json!(notice.kind)),
                    ("host".into(), json!("codex")),
                    ("message".into(), json!(notice.message)),
                    ("fault".into(), notice.fault),
                    ("runtimeEvidence".into(), notice.evidence),
                    ("executionId".into(), json!(observed.execution)),
                    ("executionGeneration".into(), json!(observed.generation)),
                ]);
                message.body_hash = Some(message.computed_hash());
                self.messages.push(message);
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }
}
