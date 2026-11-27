//! Publish live observations through the ordinary Inbox transaction.
use super::{recovery, Assignment, Inbox, Message};
use crate::workflow::{
    runtime_fault::{host_identity, last_accepted_input, Notice, Observation, ReadMode},
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
        if allowance.is_zero() {
            return Vec::new();
        }
        if let Some(id) = dispatch {
            return sample(state, &dir, id, scope).into_iter().collect();
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
                observed.extend(sample(state, &dir, id, scope));
            }
        }
        observed
    }
}

pub(super) fn sample(state: &Path, dir: &Path, id: &str, scope: Option<&str>) -> Option<Sample> {
    let generations = recovery::Generations::read(&dir, id);
    let row = generations.published()?;
    if row["dispatch"] != id
        || !row["scope"]
            .as_str()
            .is_some_and(|bound| !bound.is_empty() && scope.is_none_or(|scope| bound == scope))
    {
        return None;
    }
    let observed = Observation::read(state, row, ReadMode::Poll)?;
    Some(Sample {
        stamps: generations.stamps(),
        assignment: serde_json::from_value(row.clone()).ok()?,
        row: row.clone(),
        observed,
    })
}

fn evidence(message: &Message) -> &Value {
    message
        .additional_fields
        .get("runtimeEvidence")
        .unwrap_or(&Value::Null)
}

fn same_fault(message: &Message, notice: &Notice) -> bool {
    let prior = evidence(message);
    match notice.kind {
        "host_error" => {
            let Some(fault) = message.additional_fields.get("fault") else {
                return false;
            };
            fault["category"]
                .as_str()
                .zip(prior["code"].as_str())
                .is_some_and(|(category, code)| {
                    let status = prior["httpStatus"]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok());
                    host_identity(category, code, status) == notice.fingerprint
                })
        }
        "stall_timeout" => {
            prior["session"] == notice.evidence["session"]
                && prior["turn"] == notice.evidence["turn"]
                && (progress_offset(prior) == progress_offset(&notice.evidence)
                    || legacy_stall_at(prior)
                        .zip(notice.evidence["lastProgressAtMs"].as_u64())
                        .is_some_and(|(before, after)| after <= before))
        }
        _ => false,
    }
}

fn progress_offset(evidence: &Value) -> Option<u64> {
    evidence["progressOffset"]
        .as_u64()
        .or_else(|| evidence["offset"].as_u64())
}

fn legacy_stall_at(evidence: &Value) -> Option<u64> {
    // R1 used the file mtime. Progress already present when R1 sampled
    // the file cannot recover its stall or start a new stall episode.
    evidence["progressOffset"]
        .is_null()
        .then(|| evidence["lastProgressAtMs"].as_u64())
        .flatten()
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

            // The cursor is an optimization for bounded reads. A cache-only
            // failure must leave ordinary Inbox publications available.
            let _ = observed.save_cursor(&self.dir);
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

            let episode: Vec<_> = current
                .iter()
                .copied()
                .filter(|message| {
                    message
                        .additional_fields
                        .get("executionGeneration")
                        .and_then(Value::as_str)
                        == Some(&observed.generation)
                })
                .collect();
            let last_recovery = episode.iter().rposition(|message| {
                message
                    .additional_fields
                    .get("kind")
                    .and_then(Value::as_str)
                    == Some("runtime_recovered")
            });
            let open = &episode[last_recovery.map_or(0, |index| index + 1)..];
            let recovery = activity.and_then(|activity| {
                let last = open.iter().rev().find(|message| {
                    matches!(
                        message
                            .additional_fields
                            .get("kind")
                            .and_then(Value::as_str),
                        Some("host_error" | "stall_timeout")
                    )
                })?;
                let prior = evidence(last);
                let at = activity["atMs"].as_u64()?;
                (activity["session"] == prior["session"]
                    && activity["offset"].as_u64()? > progress_offset(prior)?
                    && at >= prior["atMs"].as_u64()?
                    && legacy_stall_at(prior).is_none_or(|seen| at > seen))
                .then(|| Notice::recovered(activity))
            });
            // Use the previous verified recovery point to distinguish episodes.
            // R1 receipts are checked by evidence as well as by request key.
            let episode_id = recovery
                .as_ref()
                .map(|notice| notice.fingerprint.clone())
                .or_else(|| {
                    last_recovery.map(|index| {
                        let point = evidence(episode[index]);
                        Notice::recovered(point.clone()).fingerprint
                    })
                })
                .unwrap_or_default();
            let notice = notice.filter(|notice| {
                recovery.is_some()
                    || !open.iter().any(|message| {
                        message
                            .additional_fields
                            .get("kind")
                            .and_then(Value::as_str)
                            == Some(notice.kind)
                            && same_fault(message, notice)
                    })
            });
            for notice in recovery.into_iter().chain(notice).chain(deadline) {
                let episode = if matches!(notice.kind, "host_error" | "stall_timeout") {
                    episode_id.as_str()
                } else {
                    ""
                };
                let key = format!(
                    "runtime:{}:{}:{}:{}:{}:{}:{}:{}",
                    worker.dispatch,
                    worker.round,
                    worker.attempt,
                    observed.execution,
                    observed.generation,
                    notice.kind,
                    notice.fingerprint,
                    episode,
                );
                let old_key = key.strip_suffix(':').unwrap_or(&key);
                if self.prior(&worker.dispatch, &key).is_some()
                    || self.prior(&worker.dispatch, old_key).is_some()
                {
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
