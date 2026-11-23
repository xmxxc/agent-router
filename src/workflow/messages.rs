//! A worker can retry a keyed message. A controller claims a batch
//! before printing it, so another waiter cannot deliver the same messages.
mod receipt;
#[cfg(target_os = "linux")]
pub mod relay;
#[cfg(target_os = "linux")]
mod reply;
pub use receipt::{Confirm, ACK_USAGE, RESOLVE_USAGE};
use receipt::{Delivery, Resolution, State};
#[cfg(target_os = "linux")]
pub use reply::{Reply, REPLY_USAGE};

use super::{controller, git_snapshot::Snapshot, resolve_report_path, state_dir, valid_id};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DONE_USAGE: &str =
    "wfc done --report FILE [--attempt ID] [--message-id KEY] [--owner-token TOKEN]";
pub const ASK_USAGE: &str =
    "wfc ask QUESTION [--options CSV] [--attempt ID] [--callback-ref REF] [--message-id KEY] [--owner-token TOKEN]";
pub const WAIT_USAGE: &str =
    "wfc wait [--timeout SECONDS] [--scope NAME] [--dispatch ID] [--owner-token TOKEN]";

pub(super) fn options<'a, const N: usize>(
    args: &'a [OsString],
    names: [&str; N],
    positional: Option<(&mut Option<&'a str>, &str)>,
) -> Result<[Option<&'a str>; N]> {
    parse_options(args, names, positional)?.context("unexpected help")
}

// None indicates an ask help request.
fn parse_options<'a, const N: usize>(
    args: &'a [OsString],
    names: [&str; N],
    mut positional: Option<(&mut Option<&'a str>, &str)>,
) -> Result<Option<[Option<&'a str>; N]>> {
    let mut values = [None; N];
    let mut tokens = args.iter();
    let question = positional
        .as_ref()
        .is_some_and(|(_, name)| *name == "question");
    let mut positional_only = false;
    while let Some(token) = tokens.next() {
        let flag = token.to_str().context("expected UTF-8 argument")?;
        if let Some((slot, name)) = positional.as_mut() {
            if flag == "--" && !positional_only {
                positional_only = true;
                continue;
            }
            if question && !positional_only && flag.starts_with("-h") {
                ensure!(!flag.starts_with("-h="), "help does not take a value");
                return Ok(None);
            }
            if positional_only
                || !flag.starts_with('-')
                || (question && literal_question_argument(flag, &names))
            {
                ensure!(
                    (!flag.is_empty() || question) && slot.replace(flag).is_none(),
                    "expected one {name}"
                );
                continue;
            }
        }
        let (flag, inline) = flag
            .split_once('=')
            .map_or((flag, None), |(flag, value)| (flag, Some(value)));
        let index = names
            .iter()
            .position(|name| flag == *name)
            .with_context(|| format!("unknown option {flag:?}"))?;
        let value = match inline {
            Some(value) => value,
            None => tokens
                .next()
                .context("option requires a value")?
                .to_str()
                .context("expected UTF-8 option value")?,
        };
        ensure!(
            !value.is_empty()
                || matches!(flag, "--body" | "--token" | "--owner-token" | "--owner")
                || (question
                    && matches!(
                        flag,
                        "--options" | "--attempt" | "--message-id" | "--callback-ref"
                    )),
            "empty option value"
        );
        let looks_like_option = if question {
            !literal_question_argument(value, &names)
        } else {
            value.starts_with("--")
                && !(positional.is_none() && matches!(flag, "--report" | "--message-id"))
        };
        ensure!(
            inline.is_some() || !looks_like_option,
            "option requires a value"
        );
        ensure!(
            values[index].replace(value).is_none(),
            "duplicate option {flag:?}"
        );
    }
    Ok(Some(values))
}

fn literal_question_argument(text: &str, names: &[&str]) -> bool {
    // Argparse recognizes attached short help and named options before
    // applying its literal-space rule.
    let option = text.split_once('=').map_or(text, |(name, _)| name);
    !text.starts_with('-')
        || short_question(text)
        || (text.contains(' ')
            && !text.starts_with("-h")
            && option != "--help"
            && !names.contains(&option))
}

// Argparse accepts "-" and negative integer or decimal text as positionals.
fn short_question(text: &str) -> bool {
    let Some(number) = text.strip_prefix('-') else {
        return false;
    };
    let digits = |part: &str| part.chars().all(decimal_digit);
    match number.split_once('.') {
        Some((whole, fraction)) => digits(whole) && !fraction.is_empty() && digits(fraction),
        None => digits(number),
    }
}

fn decimal_digit(c: char) -> bool {
    // Unicode 15.0 Nd, as used by Python 3.12's argparse negative-number
    // matcher. Each start begins a run of exactly ten consecutive digits.
    const STARTS: &[u32] = &[
        0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66,
        0x0ce6, 0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946,
        0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0,
        0xa9f0, 0xaa50, 0xabf0, 0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0,
        0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50,
        0x11da0, 0x11f50, 0x16a60, 0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6,
        0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950, 0x1fbf0,
    ];
    let point = u32::from(c);
    let next = STARTS.partition_point(|start| *start <= point);
    next.checked_sub(1).is_some_and(|i| point - STARTS[i] < 10)
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Identity {
    dispatch: String,
    round: u64,
    attempt: String,
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

impl Identity {
    fn scope(&self) -> &str {
        // Pre-scope receipts still have a stable, per-dispatch scope.
        self.scope.as_deref().unwrap_or(&self.dispatch)
    }

    fn fields(&self) -> Map<String, Value> {
        Map::from_iter([
            ("attempt".into(), json!(self.attempt)),
            ("role".into(), json!(self.role)),
            ("round".into(), json!(self.round)),
        ])
    }
}

#[derive(Deserialize)]
struct Assignment {
    #[serde(flatten)]
    worker: Identity,
    report: Option<PathBuf>,
    cwd: PathBuf,
    #[serde(rename = "controllerOwner")]
    controller_owner: Option<Value>,
    #[serde(rename = "workerCallbackHash")]
    callback_hash: Option<String>,
}

impl Assignment {
    fn read(state: &Path, dispatch: &str) -> Result<(Self, Value)> {
        let dir = state.join("dispatches");
        let (file, path) = super::assignment::open_assignment(&dir, dispatch)
            .with_context(|| format!("open dispatch {dispatch}"))?;
        let mut row = super::decode_record(file, &path)?;
        if path.extension().is_some_and(|ext| ext == "pending") {
            row["createPending"] = json!(true);
        }
        let assignment = Self::deserialize(&row).context("read worker assignment identity")?;
        ensure!(
            assignment.controller_owner.is_none(),
            "controller owner authorization required for {dispatch}"
        );
        Ok((assignment, row))
    }

    fn current(
        state: &Path,
        dispatch: &str,
        expected_attempt: Option<&str>,
    ) -> Result<(Self, Value)> {
        let (assignment, row) = Self::read(state, dispatch)?;
        let worker = &assignment.worker;
        if let Some(round) = env::var_os("WFC_ROUND") {
            ensure!(
                round.to_str().and_then(|s| s.parse::<u64>().ok()) == Some(worker.round),
                "round mismatch for {dispatch}"
            );
        }
        ensure!(
            env::var_os("WFC_ATTEMPT").is_none_or(|v| v.to_str() == Some(worker.attempt.as_str()))
                && expected_attempt.is_none_or(|id| id == worker.attempt),
            "attempt is no longer accepted for {dispatch}"
        );
        Ok((assignment, row))
    }

    // Call with the ledger lock held while checking the current assignment.
    fn authorize_callback(&self, state: &Path, token: Option<&str>, operation: &str) -> Result<()> {
        if let Some(secret) = controller::worker_callback() {
            if secret.to_str().is_some_and(|secret| {
                self.callback_hash.as_deref() == Some(body_hash(secret).as_str())
            }) {
                return Ok(());
            }
            return Err(controller::invalid_worker_callback(operation).into());
        }
        controller::Owners::read(state)?.require(self.worker.scope(), token, operation)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Message {
    id: u64,
    #[serde(rename = "messageId")]
    message_id: String,
    #[serde(flatten)]
    worker: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    question_payload: Option<Question>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_payload: Option<ReplyRecord>,
    request_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_hash: Option<String>,
    #[serde(flatten)]
    delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    // Keep additive fields when rewriting a receipt from a newer publisher.
    #[serde(flatten)]
    additional_fields: Map<String, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum MessageKind {
    Done,
    Question,
    Reply,
    Reclaimed,
}

#[derive(Serialize, Deserialize)]
struct Question {
    text: String,
    options: Vec<String>,
}

// Reply attempts live in the same ledger as the question. They are addressed
// to the worker and never appear in controller delivery or confirmation.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyRecord {
    to: u64,
    question_message_id: String,
    answer: String,
    input_result: InputResult,
    diagnostic: Option<String>,
    category: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InputResult {
    Pending,
    NotSubmitted,
    DefinitelyNotSent,
    Refused,
    Uncertain,
    Accepted,
}

#[cfg(target_os = "linux")]
impl InputResult {
    fn claims_question(self) -> bool {
        matches!(self, Self::Pending | Self::Uncertain | Self::Accepted)
    }
}

pub(super) fn body_hash(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

fn canonical_body(body: Map<String, Value>) -> String {
    // Map iterates in sorted key order, as required when hashing a body.
    spaced_json(&Value::Object(body))
}

fn spaced_json(value: &Value) -> String {
    use serde_json::ser::Formatter;
    use std::io::Result as IoResult;

    struct PythonSeparators;
    impl Formatter for PythonSeparators {
        fn begin_array_value<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> IoResult<()> {
            if !first {
                w.write_all(b", ")?;
            }
            Ok(())
        }

        fn begin_object_key<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> IoResult<()> {
            self.begin_array_value(w, first)
        }

        fn begin_object_value<W: Write + ?Sized>(&mut self, w: &mut W) -> IoResult<()> {
            w.write_all(b": ")
        }
    }

    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, PythonSeparators);
    value
        .serialize(&mut serializer)
        .expect("serialize JSON value");
    String::from_utf8(bytes).expect("JSON is UTF-8")
}

impl Question {
    fn fields(&self) -> Map<String, Value> {
        Map::from_iter([
            ("question".into(), json!(self.text)),
            ("options".into(), json!(self.options)),
        ])
    }

    fn body(&self, worker: &Identity) -> Map<String, Value> {
        let mut body = worker.fields();
        body.extend(self.fields());
        body.insert("scope".into(), json!(worker.scope()));
        body
    }

    fn key_suffix(&self) -> String {
        use std::fmt::Write as _;

        // The request key uses Python's default ASCII escaping, including
        // surrogate pairs. The separately stored body digest uses UTF-8.
        let mut ascii = String::new();
        for c in canonical_body(self.fields()).chars() {
            if c < '\u{7f}' {
                ascii.push(c);
            } else {
                for &unit in c.encode_utf16(&mut [0; 2]).iter() {
                    write!(ascii, "\\u{unit:04x}").unwrap();
                }
            }
        }
        body_hash(&ascii)[..16].to_owned()
    }
}

impl Message {
    fn new(id: u64, worker: Identity, request_key: Option<String>) -> Self {
        Self {
            id,
            message_id: format!("m-{}", Uuid::new_v4().simple()),
            worker,
            report: None,
            question_payload: None,
            reply_payload: None,
            request_key,
            body_hash: None,
            delivery: Delivery::unsent(),
            resolution: None,
            created_at: Some(receipt::timestamp()),
            additional_fields: Map::new(),
        }
    }

    fn kind(&self) -> MessageKind {
        // The worker event is explicitly tagged on disk. A report path on
        // this event is context, never a completion payload.
        if self
            .additional_fields
            .get("type")
            .is_some_and(|tag| tag == "reclaimed")
        {
            MessageKind::Reclaimed
        } else if self.reply_payload.is_some() {
            MessageKind::Reply
        } else if self.question_payload.is_some() {
            MessageKind::Question
        } else {
            MessageKind::Done
        }
    }

    fn is_done(&self) -> bool {
        self.kind() == MessageKind::Done
    }

    fn is_controller_message(&self) -> bool {
        matches!(self.kind(), MessageKind::Done | MessageKind::Question)
    }

    fn valid_payload(&self) -> bool {
        match self.kind() {
            MessageKind::Done => self.report.is_some(),
            MessageKind::Question => self.report.is_none(),
            MessageKind::Reply => self.report.is_none() && self.question_payload.is_none(),
            MessageKind::Reclaimed => {
                self.question_payload.is_none()
                    && self.reply_payload.is_none()
                    && self.request_key.is_none()
            }
        }
    }

    fn original_body(&self) -> Map<String, Value> {
        let mut body = self.worker.fields();
        if let Some(report) = &self.report {
            body.insert("report".into(), json!(report));
        }
        body
    }

    fn body(&self) -> Map<String, Value> {
        if let Some(reply) = &self.reply_payload {
            let mut body = self.worker.fields();
            body.insert("scope".into(), json!(self.worker.scope()));
            body.extend(json!(reply).as_object().unwrap().clone());
            return body;
        }
        if let Some(question) = &self.question_payload {
            return question.body(&self.worker);
        }
        let mut body = self.original_body();
        for key in ["head", "dirty"] {
            if let Some(value) = self.additional_fields.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
        // The scope-aware publisher uses the dispatch ID for older rows
        // without a scope when it writes their (possibly null) Git snapshot.
        if self.worker.scope.is_some() || body.contains_key("head") {
            body.insert("scope".into(), json!(self.worker.scope()));
        }
        body
    }

    fn computed_hash(&self) -> String {
        body_hash(&canonical_body(self.body()))
    }

    fn matches_hash(&self, hash: &str) -> bool {
        self.body_hash
            .as_deref()
            .is_none_or(|stored| stored == hash)
            && self.computed_hash() == hash
    }

    fn could_survive_legacy_write(&self) -> bool {
        // The four-field publisher read the whole inbox, requiring a string
        // request key on every row. Its rewrite dropped newer fields. Only
        // publication-time values count: later claims can bind a digest or
        // add null Git fields even on a genuine legacy row.
        self.is_done()
            && self.request_key.is_some()
            && self.worker.scope.is_none()
            && self.created_at.is_none()
            && self
                .additional_fields
                .get("head")
                .is_none_or(Value::is_null)
            && self
                .additional_fields
                .get("dirty")
                .is_none_or(Value::is_null)
    }

    fn matches_ack_body(&self, body: &str, earlier: &[Self]) -> bool {
        if self.matches_hash(&body_hash(body)) {
            return true;
        }
        // A pre-scope message could already have been received when a later
        // publisher rewrote the inbox, adding the two null Git fields. That
        // row is indistinguishable from one claimed after the rewrite. The
        // original publisher accepted only one completion per dispatch and
        // attempt, and its write had to preserve the preceding publications.
        self.body_hash.is_none()
            && self.could_survive_legacy_write()
            && self.additional_fields.contains_key("head")
            && self.additional_fields.contains_key("dirty")
            && body == canonical_body(self.original_body())
            && earlier.iter().all(|m| {
                m.could_survive_legacy_write()
                    && (m.worker.dispatch != self.worker.dispatch
                        || m.worker.attempt != self.worker.attempt)
            })
    }

    fn receipt(&self, duplicate: bool) -> Value {
        let mut receipt = json!({"message": self.id, "messageId": self.message_id, "state": self.delivery.state,
            "statusVersion": self.delivery.status_version, "duplicate": duplicate, "scope": self.worker.scope()});
        if let Some(resolution) = &self.resolution {
            receipt["resolution"] = json!(resolution);
        }
        receipt
    }

    fn view(&self) -> Value {
        self.view_at(self.delivery.state, self.delivery.status_version)
    }

    fn view_at(&self, state: State, version: u64) -> Value {
        let mut view = self.body();
        view.extend([
            ("id".into(), json!(self.id)),
            ("messageId".into(), json!(self.message_id)),
            ("dispatch".into(), json!(self.worker.dispatch)),
            ("type".into(), json!(self.kind())),
            ("state".into(), json!(state)),
            ("statusVersion".into(), json!(version)),
        ]);
        if self.kind() == MessageKind::Reclaimed {
            if let Some(tmux) = self.additional_fields.get("tmux") {
                view.insert("tmux".into(), tmux.clone());
            }
        }
        Value::Object(view)
    }

    fn claim(&mut self, state: State) -> Result<()> {
        self.delivery.advance(state)?;
        if self.body_hash.is_none() {
            self.body_hash = Some(self.computed_hash());
        }
        Ok(())
    }
}

pub(super) fn private_file() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

struct Inbox {
    dir: PathBuf,
    messages: Vec<Message>,
    // The lock lives on a stable file because save replaces the data file.
    _lock: File,
}

fn lock_file(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir).context("create message directory")?;
    let path = dir.join("messages.lock");
    let mut options = private_file();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // A stable link may use an existing lock, but must not create its target.
        let linked = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type().is_symlink(),
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        options
            .create(!linked)
            .custom_flags(libc::O_NONBLOCK | if linked { 0 } else { libc::O_NOFOLLOW });
    }
    #[cfg(not(target_os = "linux"))]
    options.create(true);
    let lock = options
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        lock.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(lock)
}

pub(super) fn lock_for_publish(dir: &Path) -> Result<File> {
    let lock = lock_file(dir)?;
    lock.lock().context("lock controller messages")?;
    Ok(lock)
}

pub(super) fn sync_directory(_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_dir)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn sticky_owner_matches(fsuid: u32, directory_uid: u32, entry_uid: u32, overflow_uid: u32) -> bool {
    fsuid != overflow_uid && (fsuid == directory_uid || fsuid == entry_uid)
}

#[cfg(target_os = "linux")]
fn check_ledger_replacement(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let path = dir.join("messages.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(
        metadata.is_file() || (metadata.file_type().is_symlink() && fs::metadata(&path)?.is_file()),
        "{} is not a regular file",
        path.display()
    );
    let parent = fs::metadata(dir)?;
    if parent.mode() & libc::S_ISVTX != 0 {
        // Sticky directories restrict only the name being replaced. Creating
        // and removing our own scratch entry cannot test that restriction.
        let credentials =
            fs::read_to_string("/proc/self/status").context("read file credentials")?;
        let uid: u32 = credentials
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|uids| uids.split_whitespace().nth(3))
            .context("missing filesystem UID")?
            .parse()?;
        // A capability bit alone cannot establish the inode's UID/GID
        // mappings. The overflow UID can also stand for an unmapped owner.
        let overflow_uid = fs::read_to_string("/proc/sys/kernel/overflowuid")
            .context("read filesystem UID overflow value")?
            .trim()
            .parse()?;
        ensure!(
            sticky_owner_matches(uid, parent.uid(), metadata.uid(), overflow_uid),
            "cannot verify sticky directory ownership for replacing {}",
            path.display()
        );
    }

    // A named mount root or immutable/append-only entry cannot be replaced.
    // The kernel may not expose these attributes on older filesystems.
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut info = std::mem::MaybeUninit::<libc::statx>::uninit();
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            name.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS,
            info.as_mut_ptr(),
        )
    };
    if rc == 0 {
        let attrs = unsafe { info.assume_init() }.stx_attributes;
        let protected =
            libc::STATX_ATTR_IMMUTABLE | libc::STATX_ATTR_APPEND | libc::STATX_ATTR_MOUNT_ROOT;
        ensure!(
            attrs & protected as u64 == 0,
            "cannot replace protected {}",
            path.display()
        );
    } else {
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EOPNOTSUPP)) {
            return Err(error)
                .with_context(|| format!("inspect replacement of {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_snapshot_size(size: usize) -> Result<()> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read message file size limit");
    }
    let limit = unsafe { limit.assume_init() };
    ensure!(
        size as u128 <= limit.rlim_cur as u128,
        "messages.pending snapshot of {size} bytes exceeds the file size limit"
    );
    Ok(())
}

impl Inbox {
    fn snapshot(dir: &Path) -> Result<Vec<Message>> {
        let path = dir.join("messages.json");
        match File::open(&path) {
            Ok(file) => {
                let mut messages: Vec<Message> = serde_json::from_reader(BufReader::new(file))
                    .context("read controller messages")?;
                // The first scoped wait on an unclaimed pre-snapshot completion
                // supplies an unknown Git snapshot. Preserve an already bound
                // body (including the earlier four-field form) byte for byte.
                for m in &mut messages {
                    ensure!(m.valid_payload(), "invalid message payload");
                    if m.is_done()
                        && m.delivery.legacy_unclaimed
                        && m.body_hash.is_none()
                        && m.worker.scope.is_none()
                        && !m.additional_fields.contains_key("head")
                        && !m.additional_fields.contains_key("dirty")
                    {
                        m.additional_fields.insert("head".into(), Value::Null);
                        m.additional_fields.insert("dirty".into(), Value::Null);
                    }
                }
                Ok(messages)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn open(dir: &Path) -> Result<Self> {
        Self::read_locked(dir, lock_for_publish(dir)?)
    }

    fn read_locked(dir: &Path, lock: File) -> Result<Self> {
        let messages = Self::snapshot(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            messages,
            _lock: lock,
        })
    }

    fn try_open(state: &Path) -> Result<Option<Self>> {
        let lock = lock_file(state)?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e).context("lock controller messages"),
        }
        Ok(Some(Self::read_locked(state, lock)?))
    }

    fn claim(
        &mut self,
        dispatch: Option<&str>,
        scope: Option<&str>,
        state: State,
    ) -> Result<Vec<Value>> {
        let batch: Vec<_> = self
            .messages
            .iter_mut()
            .filter(|m| {
                m.is_controller_message()
                    && m.delivery.state == State::Unsent
                    && dispatch.is_none_or(|id| id == m.worker.dispatch)
                    && scope.is_none_or(|scope| scope == m.worker.scope())
            })
            .map(|m| {
                m.claim(state)?;
                Ok(m.view())
            })
            .collect::<Result<_>>()?;
        if !batch.is_empty() {
            self.save().context("claim controller messages")?;
        }
        Ok(batch)
    }

    fn validate_done(
        &self,
        assignment: &Assignment,
        row: &Value,
        report: &Path,
        key: Option<&str>,
        attempt: Option<&str>,
    ) -> Result<Option<Value>> {
        let worker = &assignment.worker;
        let history = || {
            self.messages
                .iter()
                .filter(|m| m.is_done() && m.worker.dispatch == worker.dispatch)
        };
        let prior = key.and_then(|key| self.prior(&worker.dispatch, key));
        if let (Some(key), Some(prior)) = (key, prior) {
            ensure!(
                prior.is_done()
                    && prior.worker.attempt == worker.attempt
                    && prior.worker.round == worker.round
                    && prior.report.as_deref() == Some(report),
                "message request {key} conflicts with an existing receipt"
            );
        }
        let previous = history()
            .find(|m| m.worker.round == worker.round && m.worker.attempt == worker.attempt);
        if let Some(bound) = assignment
            .report
            .as_deref()
            .or_else(|| previous.and_then(|m| m.report.as_deref()))
        {
            ensure!(
                report == bound,
                "the current round reports to {}",
                bound.display()
            );
        } else {
            // Pre-binding Rust records carry no intended filename. Only the original
            // attempt can establish it, and concurrent first submissions recheck it.
            ensure!(
                attempt.is_some() || env::var_os("WFC_ATTEMPT").is_some(),
                "legacy dispatch has no bound report; supply --attempt"
            );
        }
        if let Some(prior) = prior {
            return Ok(Some(prior.receipt(true)));
        }
        let resume = super::assignment::resume_audit(row);
        if let Some(event) = resume {
            ensure!(
                event["launchResult"] != "definitely-not-sent"
                    && (event["result"] == "accepted"
                        || (attempt.is_some() && controller::worker_callback().is_some())),
                "attempt is no longer accepted for {}; its launch is unconfirmed",
                worker.dispatch
            );
        }
        if attempt.is_none() {
            let reused_report = resume.is_some()
                && row["rounds"].as_array().into_iter().flatten().any(|r| {
                    r["attempt"] != worker.attempt
                        && r["report"].as_str().map(Path::new) == Some(report)
                });
            ensure!(
                !reused_report && !history().any(|m| (m.worker.round != worker.round
                    || m.worker.attempt != worker.attempt)
                    && m.report.as_deref() == Some(report)),
                "ambiguous report for {}; supply --attempt",
                worker.dispatch
            );
            let earlier_rounds: std::collections::BTreeSet<_> = history()
                .map(|m| m.worker.round)
                .filter(|round| (1..worker.round).contains(round))
                .collect();
            ensure!(
                worker.round.checked_sub(1) == Some(earlier_rounds.len() as u64),
                "incomplete round history for {}; supply --attempt",
                worker.dispatch
            );
        }
        Ok(None)
    }

    fn prior(&self, dispatch: &str, key: &str) -> Option<&Message> {
        self.messages.iter().find(|m| {
            m.is_controller_message()
                && m.worker.dispatch == dispatch
                && m.request_key.as_deref() == Some(key)
        })
    }

    fn require_question_round(
        &self,
        assignment: &Assignment,
        row: &Value,
        attempt: Option<&str>,
    ) -> Result<()> {
        let worker = &assignment.worker;
        let dispatch = worker.dispatch.as_str();
        let messages = || {
            self.messages
                .iter()
                .filter(|m| m.is_controller_message() && m.worker.dispatch == dispatch)
        };
        let rounds = row["rounds"].as_array().into_iter().flatten();
        ensure!(
            attempt.is_some()
                || (worker.round == 1
                    && !rounds.clone().any(|r| {
                        !r["attempt"].is_null()
                            && r["attempt"] != worker.attempt
                            && r["report"] == json!(assignment.report)
                    })
                    && !messages()
                        .any(|m| { m.worker.round == 1 && m.worker.attempt != worker.attempt })),
            "attempt is no longer accepted for {dispatch}; supply --attempt"
        );
        let current_round = rounds
            .filter(|r| r["round"] == worker.round && r["attempt"] == worker.attempt)
            .last();
        let confirmed_done = messages().any(|m| {
            m.worker.round == worker.round && m.worker.attempt == worker.attempt && m.is_done()
        }) || row["messages"].as_array().into_iter().flatten().any(|m| {
            m["type"] == "done"
                && m["dispatch"] == dispatch
                && m["round"] == worker.round
                && m["attempt"] == worker.attempt
        });
        let pending_resume = attempt.is_some()
            && controller::worker_callback().is_some()
            && super::assignment::resume_audit(row).is_some_and(|event| event["result"] == "pending");
        ensure!(
            !confirmed_done
                && (pending_resume
                    || current_round.map_or(worker.round == 1, |r| r["state"] == "running")),
            "attempt is no longer accepted for {dispatch}; its round is not running"
        );
        Ok(())
    }

    fn publish(&mut self, mut message: Message) -> Result<Value> {
        message.body_hash = Some(message.computed_hash());
        let receipt = message.receipt(false);
        self.messages.push(message);
        self.save()?;
        Ok(receipt)
    }

    fn open_pending(path: &Path) -> Result<File> {
        // A crash before rename can only leave a scratch file. The next writer
        // holds the same lock and rewrites it from the committed snapshot.
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
        private_file()
            .create_new(true)
            .open(path)
            .with_context(|| format!("create {}", path.display()))
    }

    fn save(&self) -> Result<()> {
        self.save_with(|writer| Ok(serde_json::to_writer(writer, &self.messages)?))
    }

    fn save_with(
        &self,
        write_contents: impl FnOnce(&mut BufWriter<File>) -> Result<()>,
    ) -> Result<()> {
        let tmp = self.dir.join("messages.pending");
        let file = Self::open_pending(&tmp)?;
        let mut writer = BufWriter::new(file);
        write_contents(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(tmp, self.dir.join("messages.json"))?;
        self.sync_directory()
    }

    fn sync_directory(&self) -> Result<()> {
        sync_directory(&self.dir)
    }
}

// Writers publish complete snapshots by rename. A status reader can open
// either generation without taking a lock or touching the state directory.
pub(super) fn history(state: &Path, dispatch: &str) -> Result<Vec<Value>> {
    Ok(Inbox::snapshot(state)?
        .into_iter()
        .filter(|m| m.worker.dispatch == dispatch)
        .map(|m| {
            let mut view = m.view();
            match m.kind() {
                MessageKind::Done | MessageKind::Question => {
                    view["to"] = json!("controller");
                    view["delivered"] = json!(m.delivery.delivered());
                }
                MessageKind::Reclaimed => {
                    view["to"] = json!("worker");
                    view["delivered"] = json!(m.delivery.delivered());
                }
                // The reply body already carries the original question number.
                MessageKind::Reply => {}
            }
            view["at"] = json!(m.created_at);
            if let Some(resolution) = &m.resolution {
                view["resolution"] = json!(resolution);
            }
            view
        })
        .collect())
}

#[cfg(target_os = "linux")]
pub(super) struct Reclaimed {
    worker: Identity,
    tmux: Value,
    report: PathBuf,
    message_id: String,
}

#[cfg(target_os = "linux")]
impl Reclaimed {
    // Validate the ledger identity before closing a resource.
    pub(super) fn prepare(row: &Value) -> Result<Self> {
        Ok(Self {
            worker: Identity::deserialize(row).context("read reclaim assignment identity")?,
            tmux: row["tmux"].clone(),
            report: Option::<PathBuf>::deserialize(&row["report"])?.unwrap_or_default(),
            message_id: format!("m-{}", Uuid::new_v4().simple()),
        })
    }

    pub(super) fn preflight(&self, state: &Path) -> Result<()> {
        // Use the actual publisher's lock, snapshot and scratch permissions.
        // The lock is released before the caller enters the backend.
        let mut inbox = Inbox::open(state)?;
        check_ledger_replacement(state)?;
        self.append(&mut inbox);
        let bytes = serde_json::to_vec(&inbox.messages)?;
        check_snapshot_size(bytes.len())?;
        let pending = inbox.dir.join("messages.pending");
        let mut file = Inbox::open_pending(&pending)?;
        let probe = file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("verify write to {}", pending.display()));
        drop(file);
        let cleanup =
            fs::remove_file(&pending).with_context(|| format!("remove {}", pending.display()));
        probe.and(cleanup)?;
        inbox
            .sync_directory()
            .context("verify message directory sync")
    }

    pub(super) fn publish(self, state: &Path) -> Result<u64> {
        let mut inbox = Inbox::open(state)?;
        let id = self.append(&mut inbox);
        let bytes = serde_json::to_vec(&inbox.messages)?;
        check_snapshot_size(bytes.len())?;
        inbox
            .save_with(|writer| Ok(writer.write_all(&bytes)?))
            .context("publish reclaim history")?;
        Ok(id)
    }

    fn append(&self, inbox: &mut Inbox) -> u64 {
        let id = inbox.messages.len() as u64 + 1;
        inbox.messages.push(Message {
            id,
            message_id: self.message_id.clone(),
            worker: self.worker.clone(),
            report: Some(self.report.clone()),
            question_payload: None,
            reply_payload: None,
            request_key: None,
            body_hash: None,
            delivery: Delivery::unsent(),
            resolution: None,
            created_at: Some(receipt::timestamp()),
            additional_fields: Map::from_iter([
                ("type".into(), json!("reclaimed")),
                ("tmux".into(), self.tmux.clone()),
            ]),
        });
        id
    }
}

#[cfg(target_os = "linux")]
pub(super) fn reply_was_not_sent(state: &Path, row: &Value, message_id: &str) -> Result<bool> {
    // Read a complete ledger generation while the Herdr assignment is
    // locked. A pre-effect or uncertain snapshot only delays recovery.
    let worker = Identity::deserialize(row).context("read Herdr reply assignment identity")?;
    Ok(Inbox::snapshot(state)?.iter().any(|message| {
        message.message_id == message_id
            && message.worker == worker
            && message.reply_payload.as_ref().is_some_and(|reply| {
                matches!(
                    reply.input_result,
                    InputResult::DefinitelyNotSent | InputResult::Refused
                )
            })
    }))
}

pub struct Done<'a> {
    report: &'a Path,
    attempt: Option<&'a str>,
    key: Option<&'a str>,
    owner_token: Option<&'a str>,
}

impl<'a> Done<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [report, attempt, key, owner_token] = options(
            args,
            ["--report", "--attempt", "--message-id", "--owner-token"],
            None,
        )?;
        Ok(Self {
            report: Path::new(report.context("missing --report")?),
            attempt,
            key,
            owner_token,
        })
    }

    pub fn run(self) -> Result<Value> {
        let report = resolve_report_path(self.report)?
            .canonicalize()
            .with_context(|| format!("report not found: {}", self.report.display()))?;
        ensure!(report.is_file(), "report not found: {}", report.display());
        ensure!(report.to_str().is_some(), "report path must be UTF-8");
        let state = state_dir()?;
        let dispatch = env::var("WFC_DISPATCH")
            .context("run done inside a dispatched worker (WFC_DISPATCH)")?;
        ensure!(valid_id(&dispatch), "invalid dispatch ID");
        let token = controller::token(self.owner_token);
        let check = || {
            let inbox = Inbox::open(&state)?;
            let (assignment, row) = Assignment::current(&state, &dispatch, self.attempt)?;
            assignment.authorize_callback(&state, token.as_deref(), "done")?;
            // R1 dispatches had neither field and derived the default key from
            // the worker's launch environment. Assignment::current checked it.
            let r1_worker = assignment.report.is_none()
                && assignment.worker.scope.is_none()
                && env::var_os("WFC_ROUND").is_some()
                && env::var_os("WFC_ATTEMPT").is_some();
            let keyed_attempt = self
                .attempt
                .or_else(|| r1_worker.then_some(assignment.worker.attempt.as_str()));
            let key = self
                .key
                .map(str::to_owned)
                .or_else(|| keyed_attempt.map(|id| format!("done:{dispatch}:{id}")));
            let receipt =
                inbox.validate_done(&assignment, &row, &report, key.as_deref(), self.attempt)?;
            if receipt.is_none() && row["backend"] == "herdr" {
                ensure!(
                    !super::backend::herdr_close_blocks_callback(&state, &row)?,
                    "attempt {} is no longer accepted for {dispatch}; its execution resources are closing or closed",
                    assignment.worker.attempt
                );
            }
            Ok::<_, anyhow::Error>((assignment, inbox, receipt, key))
        };
        let (initial, inbox, duplicate, _) = check()?;
        if let Some(receipt) = duplicate {
            return Ok(receipt);
        }
        // Git can block. Let unrelated callbacks and waits proceed while sampling.
        drop(inbox);
        let snapshot = Snapshot::read(&initial.cwd);
        let (current, mut inbox, duplicate, key) = check()?;
        ensure!(
            initial.worker.round == current.worker.round
                && initial.worker.attempt == current.worker.attempt
                && initial.cwd == current.cwd
                && initial.worker.scope() == current.worker.scope(),
            "assignment changed while sampling {dispatch}"
        );
        if let Some(receipt) = duplicate {
            return Ok(receipt);
        }
        let mut message = Message::new(inbox.messages.len() as u64 + 1, current.worker, key);
        message.report = Some(report);
        message.additional_fields = Map::from_iter([
            ("head".into(), json!(snapshot.head)),
            ("dirty".into(), json!(snapshot.dirty)),
        ]);
        inbox.publish(message).context("publish completion")
    }
}

pub struct Ask<'a> {
    question: &'a str,
    csv: Option<&'a str>,
    attempt: Option<&'a str>,
    reference: Option<&'a str>,
    key: Option<&'a str>,
    owner_token: Option<&'a str>,
}

impl<'a> Ask<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Option<Self>> {
        let mut question = None;
        let Some([csv, attempt, reference, key, owner_token]) = parse_options(
            args,
            [
                "--options",
                "--attempt",
                "--callback-ref",
                "--message-id",
                "--owner-token",
            ],
            Some((&mut question, "question")),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            question: question.context("missing question")?,
            csv,
            attempt,
            reference: reference.filter(|reference| !reference.is_empty()),
            key: key.filter(|key| !key.is_empty()),
            owner_token,
        }))
    }

    pub fn run(self) -> Result<Value> {
        let state = state_dir()?;
        let dispatch = env::var("WFC_DISPATCH")
            .context("run ask inside a dispatched worker (WFC_DISPATCH)")?;
        ensure!(valid_id(&dispatch), "invalid dispatch ID");
        let payload = Question {
            text: self.question.to_owned(),
            options: self
                .csv
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        let attempt = self.attempt.filter(|id| !id.is_empty());
        let key = self
            .key
            .map(str::to_owned)
            .or_else(|| attempt.map(|id| format!("ask:{dispatch}:{id}:{}", payload.key_suffix())));
        let token = controller::token(self.owner_token);
        let check = || -> Result<_> {
            let inbox = Inbox::open(&state)?;
            let (assignment, row) = Assignment::current(&state, &dispatch, attempt)?;
            // A reference is for a later assignment in the same live session.
            if self.reference.is_some()
                || (self.attempt == Some("") && controller::worker_callback().is_some())
            {
                return Err(controller::invalid_worker_callback("ask").into());
            }
            assignment.authorize_callback(&state, token.as_deref(), "ask")?;
            ensure!(
                env::var_os("WFC_SCOPE")
                    .is_none_or(|v| v.to_str() == Some(assignment.worker.scope())),
                "scope mismatch for {dispatch}"
            );
            let replay = if let Some((key, prior)) = key
                .as_deref()
                .and_then(|key| inbox.prior(&dispatch, key).map(|prior| (key, prior)))
            {
                ensure!(
                    prior.question_payload.is_some()
                        && prior.matches_hash(&body_hash(&canonical_body(
                            payload.body(&assignment.worker)
                        ))),
                    "message request {key} conflicts with an existing receipt"
                );
                inbox.sync_directory().context("sync question receipt")?;
                Some(prior.receipt(true))
            } else {
                inbox.require_question_round(&assignment, &row, attempt)?;
                // The Herdr close intent is saved under the inbox lock.
                if row["backend"] == "herdr" {
                    ensure!(
                        !super::backend::herdr_close_blocks_callback(&state, &row)?,
                        "attempt is no longer accepted for {dispatch}; its execution resources are closing or closed"
                    );
                }
                None
            };
            Ok((inbox, assignment, row, replay))
        };
        let (inbox, initial, row, replay) = check()?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
        // A new tmux or controlled-worker question holds the same attempt lock
        // as close from observation through publication. Acquire it outside
        // the inbox so a slow backend or close cannot block other scopes.
        drop(inbox);
        let hosted = row["host"].is_string();
        let controlled = cfg!(target_os = "linux") && !hosted && row["worker"].is_object();
        let _close_lock = (row["backend"] == "tmux" || controlled)
            .then(|| super::assignment::close_lock(&state, &row))
            .transpose()?;
        let blocked = if hosted {
            let mut inspected = row.clone();
            super::backend::observe(&state, &mut inspected).map(|(worker, _)| {
                matches!(
                    worker["state"].as_str(),
                    Some("exited" | "cancelled" | "not_started")
                )
            })
        } else {
            #[cfg(target_os = "linux")]
            let blocked = if controlled {
                super::worker_owner::question_blocked(&state.join("dispatches"), &row)
            } else {
                Ok(false)
            };
            #[cfg(not(target_os = "linux"))]
            let blocked = Ok(false);
            blocked
        };
        let (mut inbox, assignment, current, replay) = check()?;
        if let Some(replay) = replay {
            return Ok(replay);
        }
        ensure!(
            initial.worker == assignment.worker
                && ["host", "backend", "tmuxSocket", "endpoint", "executionId"]
                    .iter()
                    .all(|key| row[*key] == current[*key]),
            "assignment changed while inspecting {dispatch}"
        );
        ensure!(
            !blocked?,
            "attempt is no longer accepted for {dispatch}; its round is not running"
        );
        let mut message = Message::new(inbox.messages.len() as u64 + 1, assignment.worker, key);
        message.question_payload = Some(payload);
        let mut receipt = inbox.publish(message).context("publish question")?;
        receipt["next"] = json!("End your turn now; the answer arrives as your next message.");
        Ok(receipt)
    }
}

pub struct Wait<'a> {
    timeout: Duration,
    dispatch: Option<&'a str>,
    scope: Option<&'a str>,
    owner_token: Option<&'a str>,
}

impl<'a> Wait<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [timeout, dispatch, scope, owner_token] = options(
            args,
            ["--timeout", "--dispatch", "--scope", "--owner-token"],
            None,
        )?;
        let seconds: f64 = timeout
            .unwrap_or("3600")
            .parse()
            .context("invalid timeout")?;
        ensure!(seconds.is_finite(), "expected a finite timeout");
        let timeout = Duration::try_from_secs_f64(seconds.max(0.0)).context("invalid timeout")?;
        ensure!(dispatch.is_none_or(valid_id), "invalid dispatch ID");
        Ok(Self {
            timeout,
            dispatch,
            scope,
            owner_token,
        })
    }

    pub fn run(self) -> Result<Vec<Value>> {
        let state = state_dir()?;
        let token = controller::token(self.owner_token);
        let start = Instant::now();
        loop {
            let preflight = || {
                controller::Owners::read(&state)?.preflight(self.scope, token.as_deref(), "wait")
            };
            preflight()?;
            let batch = match Inbox::try_open(&state)? {
                Some(mut inbox) => {
                    preflight()?;
                    inbox.claim(self.dispatch, self.scope, State::Received)?
                }
                None => Vec::new(),
            };
            let remaining = self.timeout.saturating_sub(start.elapsed());
            if !batch.is_empty() || remaining.is_zero() {
                return Ok(batch);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::sticky_owner_matches;

    #[test]
    fn sticky_replacement_requires_confirmed_owner() {
        assert!(sticky_owner_matches(1001, 1001, 0, 65534));
        assert!(sticky_owner_matches(1001, 0, 1001, 65534));
        assert!(!sticky_owner_matches(1001, 0, 0, 65534));
        assert!(!sticky_owner_matches(65534, 65534, 0, 65534));
        assert!(!sticky_owner_matches(65534, 0, 65534, 65534));
        assert!(!sticky_owner_matches(1001, 1001, 1001, 1001));
    }
}
