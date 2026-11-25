//! Recover the selected tmux startup result through the ordinary inbox.
mod progress;

use super::{message_boundary, Assignment, Identity, Inbox};
use crate::workflow::{assignment, backend, decode_record, startup, valid_id};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, Metadata, OpenOptions, ReadDir},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::{Duration, Instant},
};

const RECORDS_PER_SLICE: usize = 16;
pub(super) const SLICE: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    device: u64,
    inode: u64,
    mode: u32,
    changed: (i64, i64),
    len: u64,
}

impl Stamp {
    fn new(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            len: metadata.len(),
        }
    }

    pub(super) fn at(path: &Path) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(Self::new(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

struct Generation {
    row: Value,
    stamp: Stamp,
}

impl Generation {
    fn read(path: &Path) -> Result<Option<Self>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file(), "invalid dispatch generation");
        Ok(Some(Self {
            stamp: Stamp::new(&metadata),
            row: decode_record(file, path)?,
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct GenerationStamps {
    pending: Option<Stamp>,
    published: Option<Stamp>,
}

impl GenerationStamps {
    pub(super) fn at(dir: &Path, id: &str) -> io::Result<Self> {
        Ok(Self {
            pending: Stamp::at(&dir.join(format!("{id}.pending")))?,
            published: Stamp::at(&dir.join(format!("{id}.json")))?,
        })
    }

    pub(super) fn current(self, dir: &Path, id: &str) -> bool {
        Self::at(dir, id).is_ok_and(|stamp| stamp == self)
    }
}

pub(super) struct Generations {
    pending: Option<Generation>,
    published: Option<Generation>,
    pub(super) damaged: bool,
}

impl Generations {
    pub(super) fn read(dir: &Path, id: &str) -> Self {
        // Read the reservation first to cover a concurrent promotion.
        let pending = Generation::read(&dir.join(format!("{id}.pending")));
        let published = Generation::read(&dir.join(format!("{id}.json")));
        let mut damaged = pending.is_err() || published.is_err();
        let pending = pending.ok().flatten();
        let published = published.ok().flatten();
        damaged |= pending
            .as_ref()
            .is_some_and(|generation| !assignment::valid_reservation(&generation.row));
        if let (Some(a), Some(b)) = (&pending, &published) {
            damaged |= [
                "attempt",
                "round",
                "scope",
                "controllerOwner",
                "backend",
                "executionId",
            ]
            .iter()
            .any(|&key| a.row[key] != b.row[key]);
        }
        Self {
            pending,
            published,
            damaged,
        }
    }

    pub(super) fn rows(&self) -> impl Iterator<Item = &Value> {
        self.pending.iter().chain(&self.published).map(|g| &g.row)
    }

    pub(super) fn assigned(&self) -> Option<&Value> {
        if self.damaged {
            return None;
        }
        self.published
            .as_ref()
            .or(self.pending.as_ref())
            .map(|g| &g.row)
    }

    pub(super) fn stamps(&self) -> GenerationStamps {
        GenerationStamps {
            pending: self.pending.as_ref().map(|g| g.stamp),
            published: self.published.as_ref().map(|g| g.stamp),
        }
    }

    fn published(&self) -> Option<&Value> {
        self.published
            .as_ref()
            .filter(|_| !self.damaged)
            .map(|generation| &generation.row)
    }
}

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

fn dispatch_filename(name: &[u8]) -> Option<&str> {
    std::str::from_utf8(name)
        .ok()?
        .strip_suffix(".json")
        .filter(|id| valid_id(id))
}

fn generation(worker: &Identity) -> (&str, u64, &str) {
    (&worker.dispatch, worker.round, &worker.attempt)
}

impl Inbox {
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
