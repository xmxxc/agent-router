//! Advance a verified rollout in bounded slices, resuming from a signed cursor.
use super::{classify, epoch_us, parse_timestamp_us, ReadMode, TurnEvent};
use crate::{
    bundled::RuntimeBinding,
    workflow::{codex_rollout, ensure_state_writable, write_json},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

const POLL_BUDGET: u64 = 1024 * 1024;
const MAX_RECORD: u64 = 4 * 1024 * 1024;
const STATUS_BUDGET: u64 = 8 * 1024 * 1024;
const TRAILER: u64 = 256;
const CURSOR_FORMAT: &str = "runtime-rollout-v2";

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Point {
    pub at: u64,
    pub offset: u64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Turn {
    pub id: String,
    pub started: u64,
    pub latest: Point,
    pub progress: Point,
    pub event: TurnEvent,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Activity {
    pub id: String,
    pub point: Point,
}

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    offset: u64,
    trailer: String,
    discarding: bool,
    lost_anchor: bool,
    turn: Option<Turn>,
    activity: Option<Activity>,
}

pub(super) struct Slice {
    path: PathBuf,
    identity: String,
    cursor: Cursor,
    changed: bool,
    caught_up: bool,
}

impl Slice {
    pub fn read(
        path: PathBuf,
        identity: String,
        binding: &RuntimeBinding,
        file: &mut File,
        len: u64,
        started: u64,
        mode: ReadMode,
    ) -> Option<Self> {
        let mut cursor = load(&path, &identity, binding)
            .filter(|cursor| {
                cursor.offset <= len
                    && trailer(file, cursor.offset).as_ref() == Some(&cursor.trailer)
            })
            .unwrap_or_default();
        let prior = cursor.offset;
        file.seek(SeekFrom::Start(prior)).ok()?;
        let budget = match mode {
            ReadMode::Poll => POLL_BUDGET,
            ReadMode::Status => STATUS_BUDGET,
        };
        let remaining = len - prior;
        let mut window = remaining.min(budget);
        let mut bytes = Vec::new();
        (&mut *file).take(window).read_to_end(&mut bytes).ok()?;
        // A poll at the start of a long record can finish that record without
        // raising the budget for ordinary multi-record catch-up.
        if window < remaining
            && budget == POLL_BUDGET
            && !cursor.discarding
            && bytes.len() as u64 == window
            && !bytes.contains(&b'\n')
        {
            let extended = remaining.min(MAX_RECORD);
            file.take(extended - window).read_to_end(&mut bytes).ok()?;
            window = extended;
        }
        if bytes.len() as u64 != window {
            return None;
        }
        let sampled = epoch_us(SystemTime::now())?;
        let mut caught_up = prior + window == len;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                if cursor.discarding || line.len() as u64 >= MAX_RECORD {
                    cursor.discard_oversize();
                    cursor.discarding = true;
                    cursor.offset += line.len() as u64;
                }
                break;
            }
            cursor.offset += line.len() as u64;
            if std::mem::take(&mut cursor.discarding) {
                continue;
            }
            if line.len() as u64 > MAX_RECORD {
                cursor.discard_oversize();
                continue;
            }
            cursor.observe(line, started, sampled);
        }
        // A record beyond the size limit may contain a turn boundary. Wait
        // for a new verified start before trusting later evidence.
        caught_up &= !cursor.lost_anchor;
        let changed = cursor.offset != prior;
        if changed {
            cursor.trailer = trailer(file, cursor.offset)?;
        }
        Some(Self {
            path,
            identity,
            cursor,
            changed,
            caught_up,
        })
    }

    pub fn turn(&self) -> Option<&Turn> {
        self.caught_up
            .then_some(self.cursor.turn.as_ref())
            .flatten()
    }

    pub fn incomplete(&self) -> bool {
        !self.caught_up
    }

    pub fn activity(&self) -> Option<&Activity> {
        self.caught_up
            .then_some(self.cursor.activity.as_ref())
            .flatten()
    }

    pub fn save(&self, state: &Path, binding: &RuntimeBinding, file: &File) -> Result<()> {
        if self.changed
            && !load(&self.path, &self.identity, binding).is_some_and(|cursor| {
                cursor.offset >= self.cursor.offset
                    && file
                        .try_clone()
                        .ok()
                        .and_then(|mut file| trailer(&mut file, cursor.offset))
                        .as_ref()
                        == Some(&cursor.trailer)
            })
        {
            ensure_state_writable(state)?;
            let body = json!([CURSOR_FORMAT, self.identity, self.cursor]);
            let signature = binding.sign_cursor(&serde_json::to_vec(&body)?);
            write_json(
                &self.path,
                &json!({"body":body,"signature":signature}),
                None,
            )?;
        }
        Ok(())
    }
}

impl Cursor {
    fn invalidate(&mut self) {
        self.turn = None;
        self.activity = None;
    }

    fn discard_oversize(&mut self) {
        self.invalidate();
        self.lost_anchor = true;
    }

    fn observe(&mut self, line: &[u8], started: u64, sampled: u64) {
        let Some(entry) = codex_rollout::record(line) else {
            self.invalidate();
            return;
        };
        if entry["type"] != "event_msg" {
            return;
        }
        let p = &entry["payload"];
        let kind = p["type"].as_str().unwrap_or_default();
        if kind == "thread_rolled_back" {
            self.invalidate();
            return;
        }
        if !matches!(
            kind,
            "task_started" | "task_complete" | "turn_aborted" | "item_completed"
        ) {
            return;
        }
        let Some(id) = p["turn_id"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            if kind == "turn_aborted" {
                self.invalidate();
            }
            return;
        };
        let id = id.to_string();
        if !p["root_turn_id"].is_null() && p["root_turn_id"] != id {
            return;
        }
        let Some(at) = entry["timestamp"].as_str().and_then(parse_timestamp_us) else {
            self.invalidate();
            return;
        };
        if at > sampled {
            self.invalidate();
            return;
        }
        let point = Point {
            at,
            offset: self.offset,
        };
        if kind == "task_started" {
            if self
                .turn
                .as_ref()
                .is_none_or(|turn| turn.id != id && at >= turn.latest.at)
            {
                self.turn = Some(Turn {
                    id,
                    started: at,
                    latest: point,
                    progress: point,
                    event: TurnEvent::Active,
                });
                if at > started {
                    self.lost_anchor = false;
                }
            }
            return;
        }
        let Some(turn) = self.turn.as_mut().filter(|turn| {
            turn.id == id && at >= turn.latest.at && matches!(turn.event, TurnEvent::Active)
        }) else {
            return;
        };
        match kind {
            "task_complete" => {
                turn.latest = point;
                if p["error"].is_null() {
                    turn.event = TurnEvent::Complete;
                    turn.progress = point;
                } else {
                    turn.event = classify(&p["error"]["codex_error_info"]);
                    return;
                }
            }
            "turn_aborted" => {
                turn.latest = point;
                turn.event = TurnEvent::Aborted;
                return;
            }
            // The fixed protocol gives item_completed the owning turn ID.
            // User messages, generic transcript data and housekeeping events
            // cannot establish that an interrupted turn made progress again.
            "item_completed" if progress_item(&p["item"]) => {
                turn.latest = point;
                turn.progress = point;
            }
            _ => return,
        }
        if turn.started > started {
            self.activity = Some(Activity { id, point });
        }
    }
}

fn progress_item(item: &Value) -> bool {
    item["id"].as_str().is_some_and(|id| !id.is_empty())
        && match item["type"].as_str() {
            Some("AgentMessage") => item["content"].is_array(),
            Some("Plan") => item["text"].is_string(),
            Some("Reasoning") => item["summary_text"].is_array() || item["raw_content"].is_array(),
            Some("CommandExecution") => {
                item["status"] == "completed"
                    && item["exit_code"].as_i64().is_some()
                    && item["command"].as_array().is_some_and(|command| {
                        command
                            .first()
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.is_empty())
                            && command.iter().all(Value::is_string)
                    })
            }
            Some("FunctionCallOutput") => {
                item["name"].as_str().is_some_and(|name| !name.is_empty())
                    && (item["output"].is_string() || item["output"].is_array())
            }
            _ => false,
        }
}

fn trailer(file: &mut File, offset: u64) -> Option<String> {
    let len = offset.min(TRAILER);
    file.seek(SeekFrom::Start(offset - len)).ok()?;
    let mut bytes = vec![0; len as usize];
    file.read_exact(&mut bytes).ok()?;
    let hash = sha2::Sha256::digest(&bytes);
    Some(format!("{hash:x}"))
}

fn load(path: &Path, identity: &str, binding: &RuntimeBinding) -> Option<Cursor> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(8192).read_to_end(&mut bytes).ok()?;
    let signed: Value = serde_json::from_slice(&bytes).ok()?;
    let body = &signed["body"];
    let signature = signed["signature"].as_str()?;
    if body[0] != CURSOR_FORMAT
        || body[1] != identity
        || !binding.verifies_cursor(&serde_json::to_vec(body).ok()?, signature)
    {
        return None;
    }
    serde_json::from_value(body[2].clone()).ok()
}
