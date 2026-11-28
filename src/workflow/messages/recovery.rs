//! Recover the selected tmux startup result through the ordinary inbox.
mod progress;

use super::generations::{GenerationStamps, Generations};
use super::{message_boundary, Assignment, Identity, Inbox};
use crate::workflow::{
    assignment, backend, controller, ensure_state_writable, record_scope, startup, valid_id,
};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, ReadDir},
    io,
    path::Path,
    time::{Duration, Instant},
};

const RECORDS_PER_SLICE: usize = 16;
pub(super) const SLICE: Duration = Duration::from_millis(5);

pub(super) struct Candidate {
    stamps: GenerationStamps,
    assignment: Assignment,
    host: Value,
    receipt: Value,
}

#[derive(Default)]
pub(super) struct Scan {
    entries: Option<ReadDir>,
    resolved: HashMap<String, GenerationStamps>,
    committed_poll: Option<progress::Committed>,
}

impl Scan {
    pub(super) fn in_progress(&self) -> bool {
        self.entries.is_some()
    }

    pub(super) fn batch(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
        allowance: Duration,
    ) -> Vec<Candidate> {
        let start = Instant::now();
        let allowance = allowance.min(SLICE);
        self.read(
            state,
            dispatch,
            scope,
            || start.elapsed() < allowance,
            RECORDS_PER_SLICE,
        )
    }

    fn read(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
        mut within_budget: impl FnMut() -> bool,
        limit: usize,
    ) -> Vec<Candidate> {
        let dir = state.join("dispatches");
        let mut batch = Vec::new();
        if !within_budget() {
            return batch;
        }
        if let Some(id) = dispatch {
            batch.extend(self.candidate(state, &dir, id, scope));
            return batch;
        }
        if self.entries.is_none() {
            self.entries = fs::read_dir(&dir).ok();
        }
        for _ in 0..limit {
            if !within_budget() {
                break;
            }
            let Some(entry) = self.entries.as_mut().and_then(Iterator::next) else {
                self.entries = None;
                break;
            };
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            if let Some(id) = dispatch_filename(name.as_encoded_bytes()) {
                batch.extend(self.candidate(state, &dir, id, scope));
            }
        }
        batch
    }

    fn candidate(
        &self,
        state: &Path,
        dir: &Path,
        id: &str,
        scope: Option<&str>,
    ) -> Option<Candidate> {
        if self
            .resolved
            .get(id)
            .is_some_and(|stamps| stamps.current(dir, id))
        {
            return None;
        }
        let generations = Generations::read(dir, id);
        let row = generations.published()?;
        if assignment::tmux_history(row)
            || !assignment::identified(row)
            || row["createPending"] == true
            || !row["publicationDiagnostic"].is_null()
            || !row["controllerOwner"].is_null()
            || !row["scope"]
                .as_str()
                .is_some_and(|bound| !bound.is_empty() && scope.is_none_or(|scope| scope == bound))
        {
            return None;
        }
        let receipt = backend::verified_unconfirmed_tmux_receipt(state, row).ok()??;
        Some(Candidate {
            stamps: generations.stamps(),
            assignment: Assignment::deserialize(row).ok()?,
            host: row["host"].clone(),
            receipt,
        })
    }
}

pub(super) fn dispatch_filename(name: &[u8]) -> Option<&str> {
    std::str::from_utf8(name)
        .ok()?
        .strip_suffix(".json")
        .filter(|id| valid_id(id))
}

fn generation(worker: &Identity) -> (&str, u64, &str) {
    (&worker.dispatch, worker.round, &worker.attempt)
}

impl Inbox {
    // The caller holds the scope and execution locks as well as this Inbox.
    pub(in crate::workflow) fn finish_hosted_publication(
        &self,
        id: &str,
        current: &Value,
        token: Option<&str>,
        operation: &str,
    ) -> Result<()> {
        let dir = self.dir.join("dispatches");
        let generations = Generations::read(&dir, id);
        ensure!(
            generations.published() == Some(current),
            "{}",
            assignment::UNVERIFIED_ASSIGNMENT
        );
        let Some(reserved) = generations.pending() else {
            return Ok(());
        };
        let create = &current["create"];
        // Launch evidence and ledger history belong to the published record.
        // Apart from those additions, require the whole original reservation.
        let published_only = ["create", "backendHandle", "audit", "rounds", "messages"];
        ensure!(
            current["round"] == 1
                && current["publicationDiagnostic"].is_null()
                && current["created"].is_number()
                && reserved["worker"] == serde_json::json!({"state":"running"})
                && current["worker"]["state"] == "running"
                && reserved["createPending"].is_null()
                && reserved["host"] == current["host"]
                && reserved[controller::CALLBACK_HASH] == current[controller::CALLBACK_HASH]
                && create["result"] == "accepted"
                && ["dispatch", "round", "attempt"]
                    .iter()
                    .all(|&key| create[key] == current[key])
                && ["scope", "role", "cwd", "task", "report"]
                    .iter()
                    .all(|&key| reserved[key].as_str().is_some_and(|s| !s.is_empty()))
                && reserved.as_object().is_some_and(|fields| fields
                    .iter()
                    .all(|(key, value)| !published_only.contains(&key.as_str())
                        && (key == "worker" || current.get(key) == Some(value))))
                && current
                    .as_object()
                    .is_some_and(|fields| fields
                        .iter()
                        .all(|(key, value)| published_only.contains(&key.as_str())
                            || key == "worker"
                            || reserved.get(key) == Some(value))),
            "{}",
            assignment::UNVERIFIED_ASSIGNMENT
        );
        // The launcher may have stopped before syncing its published entry.
        // Make that entry durable before removing the original reservation.
        super::sync_directory(&dir)?;
        ensure_state_writable(&self.dir)?;
        let scope = record_scope(current).context("invalid dispatch scope")?;
        controller::Owners::read(&self.dir)?.require(scope, token, operation)?;
        ensure!(
            generations.stamps().current(&dir, id),
            "{}",
            assignment::UNVERIFIED_ASSIGNMENT
        );
        assignment::finish_publication(&dir.join(format!("{id}.pending")))
    }

    pub(super) fn recover_startups(
        &mut self,
        scan: &mut Scan,
        batch: Vec<Candidate>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let dir = self.dir.join("dispatches");
        let mut missing: HashSet<_> = batch
            .iter()
            .map(|c| generation(&c.assignment.worker))
            .collect();
        for message in self
            .messages
            .iter()
            .filter(|message| message.is_controller_message())
        {
            missing.remove(&generation(&message.worker));
            if missing.is_empty() {
                break;
            }
        }
        let missing: Vec<_> = batch
            .iter()
            .map(|c| missing.contains(&generation(&c.assignment.worker)))
            .collect();

        let mut resolved = Vec::new();
        let mut published = HashSet::new();
        for (candidate, absent) in batch.into_iter().zip(missing) {
            let worker = &candidate.assignment.worker;
            // Both versions (including an absent reservation) must still
            // match. A changed record can be inspected in the next sweep.
            if !candidate.stamps.current(&dir, &worker.dispatch)
                || worker
                    .refusal(&message_boundary(&dir, &worker.dispatch))
                    .is_some()
            {
                continue;
            }
            if absent {
                let launch = candidate.receipt["launchId"]
                    .as_str()
                    .context("missing launch ID")?;
                let Ok(Some(failure)) = startup::tmux_failure(launch, &candidate.receipt) else {
                    continue;
                };
                let key = (
                    worker.dispatch.clone(),
                    worker.round,
                    worker.attempt.clone(),
                );
                resolved.push((worker.dispatch.clone(), candidate.stamps));
                if published.insert(key) {
                    self.push_startup_failure(candidate.assignment, &candidate.host, &failure)?;
                }
            } else {
                resolved.push((worker.dispatch.clone(), candidate.stamps));
            }
        }
        if !published.is_empty() {
            self.save().context("recover confirmed startup failure")?;
        }
        scan.resolved.extend(resolved);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn callback_assignment_uses_consistent_publication_generations() -> Result<()> {
        let state =
            std::env::temp_dir().join(format!("wfc-publication-generation-{}", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            fs::create_dir_all(&state)?;
            let dispatch = "d-00000031";
            let pending = state.join(format!("{dispatch}.pending"));
            let published = state.join(format!("{dispatch}.json"));
            let row = json!({"dispatch":dispatch,"round":1,"attempt":"a-000000000031",
                "scope":"fixture","role":"worker","host":"codex","backend":"herdr"});
            fs::write(&pending, serde_json::to_vec(&row)?)?;
            let generations = Generations::read(&state, dispatch);
            assert!(generations.message_boundary().refusal(None, None).is_none());
            assert_eq!(generations.into_assigned()?.unwrap()["createPending"], true);

            let mut current = row.clone();
            current["create"] = json!({"result":"accepted"});
            fs::write(&published, serde_json::to_vec(&current)?)?;
            assert_eq!(
                Generations::read(&state, dispatch).into_assigned()?,
                Some(current)
            );

            for (field, value) in [
                ("backend", json!("bundled")),
                ("executionId", json!("a-000000000032")),
                ("role", json!("reviewer")),
                ("scope", json!("conflicting-publication")),
                ("round", json!(2)),
            ] {
                let mut conflict = row.clone();
                conflict[field] = value;
                fs::write(&pending, serde_json::to_vec(&conflict)?)?;
                let generations = Generations::read(&state, dispatch);
                assert_eq!(
                    generations.message_boundary().refusal(None, None),
                    Some(assignment::UNVERIFIED_ASSIGNMENT)
                );
                assert!(generations.into_assigned().is_err(), "{field}");
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(state);
        result
    }

    #[test]
    fn startup_recovery_skips_retired_attempts_in_small_slices() -> Result<()> {
        let state = std::env::temp_dir().join(format!("wfc-startup-slices-{}", Uuid::new_v4()));
        let result = (|| {
            let dir = state.join("dispatches");
            fs::create_dir_all(&dir)?;
            fs::create_dir(state.join("launches"))?;
            let state = state.canonicalize()?;
            let dir = state.join("dispatches");
            let write = |path: &Path, row: Value| crate::workflow::write_json(path, &row, None);
            let scope = "bounded-recovery";
            let attempt = "a-123456789abc";
            let report = state.join("report");
            for index in 1..=7 {
                let dispatch = format!("d-{index:08x}");
                let launch = format!("{index:032x}");
                let receipt = state.join("launches").join(format!("{launch}.json"));
                write(
                    &receipt,
                    json!({"category":"process_exited","exitCode":17,"launchId":launch,
                    "startupExit":{"exitCode":17,"signal":null,"pid":123,"processStart":null}}),
                )?;
                write(
                    &dir.join(format!("{dispatch}.json")),
                    json!({
                        "dispatch":dispatch,"round":1,"attempt":attempt,"scope":scope,
                        "executionId":attempt,"role":"worker","cwd":state,"report":report,
                        "backend":"tmux","host":"codex","create":{"result":"uncertain",
                            "category":"process_exited","dispatch":dispatch,"round":1,
                            "attempt":attempt,"exitCode":17,"receipt":receipt}
                    }),
                )?;
            }

            // Establish history in the order returned by this directory, without
            // relying on filenames or the host filesystem's ordering.
            let order: Vec<_> = fs::read_dir(&dir)?
                .map(|entry| {
                    entry.map(|entry| {
                        entry
                            .path()
                            .file_stem()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned()
                    })
                })
                .collect::<io::Result<_>>()?;
            let (target, prefix) = order.split_last().unwrap();
            let history: Vec<_> = prefix
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    json!({"id":index+1,"messageId":format!("m-history-{index}"),"dispatch":id,
                    "round":1,"attempt":attempt,"scope":scope,"role":"worker","report":report,
                    "state":"processed","statusVersion":3})
                })
                .collect();
            write(&state.join("messages.json"), json!(history))?;

            let before = fs::read(state.join("messages.json"))?;
            let mut scan = Scan::default();
            assert!(scan.read(&state, None, Some(scope), || false, 2).is_empty());
            assert!(!scan.in_progress());
            let mut clock_checks = 0;
            assert!(scan
                .read(
                    &state,
                    None,
                    Some(scope),
                    || {
                        clock_checks += 1;
                        clock_checks <= 3
                    },
                    16
                )
                .is_empty());
            assert_eq!(clock_checks, 4);
            for _ in 0..2 {
                assert!(scan.in_progress());
                assert!(scan.read(&state, None, Some(scope), || true, 2).is_empty());
            }
            assert!(scan.in_progress());
            let slice = scan.read(&state, None, Some(scope), || true, 2);
            assert!(slice.is_empty());
            assert!(!scan.in_progress());
            let mut inbox = Inbox::open(&state)?;
            inbox.recover_startups(&mut scan, slice)?;
            assert_eq!(inbox.messages.len(), prefix.len());
            assert!(inbox
                .claim(None, Some(scope), super::super::State::Received)?
                .is_empty());

            let mut restarted = Scan::default();
            assert!(restarted
                .read(&state, Some(target), Some(scope), || true, 2)
                .is_empty());
            assert_eq!(fs::read(state.join("messages.json"))?, before);
            Ok(())
        })();
        let _ = fs::remove_dir_all(state);
        result
    }
}
