//! Publish a complete native archive for an attested, stopped SQLite copy.
mod history;

use super::{controller, host, messages, valid_id, valid_identity, write_json};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use wfc_legacy_state::stopped_ledger;

pub const USAGE: &str = "wfc migrate-legacy --offline-copy SOURCE --stop-proof FILE";
const MARKER: &str = "legacy-import.json";
const ARCHIVE: &str = "legacy-source";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StopProof {
    source_sha256: String,
    target: PathBuf,
    writers_stopped: bool,
    stopped_by: String,
    evidence: String,
    #[serde(default)]
    owner_replacements: BTreeMap<String, controller::LegacyOwnerReplacement>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Import {
    version: u32,
    state: PathBuf,
    source: PathBuf,
    source_sha256: String,
    stop_proof_sha256: String,
    stopped_by: String,
    evidence: String,
    message_high_water: u64,
    dispatch_count: usize,
    message_count: usize,
    owners: Vec<Value>,
}

pub(super) fn text<'a>(row: &'a Value, key: &str) -> Result<&'a str> {
    row[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing or invalid legacy {key}"))
}

pub(super) fn sha256_text(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn published(state: &Path) -> Result<Option<Import>> {
    let path = state.join(MARKER);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open legacy import publication record"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "invalid legacy import publication record"
    );
    let import: Import =
        serde_json::from_reader(file).context("read legacy import publication record")?;
    ensure!(
        import.version == 1 && import.state == state.canonicalize()?,
        "legacy import staging has no published writer at this path"
    );
    for name in ["messages.json", "controller-owners.json"] {
        ensure!(
            fs::symlink_metadata(state.join(name)).is_ok_and(|entry| entry.is_file()),
            "published legacy import is missing a regular {name}"
        );
    }
    ensure!(
        fs::symlink_metadata(state.join("dispatches")).is_ok_and(|entry| entry.is_dir()),
        "published legacy import is missing its dispatch directory"
    );
    Ok(Some(import))
}

pub(super) fn high_water(state: &Path) -> Result<u64> {
    Ok(published(state)?.map_or(0, |import| import.message_high_water))
}

fn no_sidecars(directory: &Path) -> Result<()> {
    for suffix in ["wal", "shm", "journal"] {
        match fs::symlink_metadata(directory.join(format!("router.db-{suffix}"))) {
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect offline SQLite recovery files"),
            Ok(_) => anyhow::bail!("offline SQLite copy has a recovery sidecar ({suffix})"),
        }
    }
    Ok(())
}

fn fingerprint(directory: &Path, mut copy: Option<&mut File>) -> Result<String> {
    no_sidecars(directory)?;
    let path = directory.join("router.db");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOATIME)
        .open(&path)
        .context("open offline router.db")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.nlink() == 1,
        "offline router.db must be an independent regular file"
    );
    let mut header = [0; 100];
    file.read_exact(&mut header)
        .context("read offline SQLite header")?;
    ensure!(
        &header[..16] == b"SQLite format 3\0" && header[18..20] == [1, 1],
        "offline copy must be a SQLite rollback-mode database"
    );
    let mut hash = Sha256::new();
    hash.update(header);
    if let Some(output) = copy.as_deref_mut() {
        output.write_all(&header)?;
    }
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        if let Some(output) = copy.as_deref_mut() {
            output.write_all(&buffer[..count])?;
        }
    }
    let after = fs::symlink_metadata(&path)?;
    ensure!(
        after.is_file()
            && after.nlink() == 1
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "offline router.db changed while reading"
    );
    no_sidecars(directory)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn private_dir(path: &Path) -> Result<()> {
    Ok(DirBuilder::new().mode(0o700).create(path)?)
}

fn parent_path() -> Result<(PathBuf, OsString)> {
    let configured =
        std::env::var_os("WFC_STATE").context("set an explicit absolute WFC_STATE for import")?;
    let target = Path::new(&configured);
    ensure!(
        target.is_absolute()
            && !matches!(
                target.components().next_back(),
                Some(std::path::Component::ParentDir)
            ),
        "import requires an absolute WFC_STATE naming a new directory"
    );
    let name = target
        .file_name()
        .context("import target needs a directory name")?
        .to_owned();
    let parent = target
        .parent()
        .context("import target needs a parent")?
        .canonicalize()?;
    ensure!(
        parent.is_dir(),
        "import target parent must be an existing directory"
    );
    Ok((parent, name))
}

fn convert_round(mut round: Value) -> Result<Value> {
    let result = &round["result"];
    if let Some(raw) = result.as_str() {
        let raw = raw.to_owned();
        round["result"] = serde_json::from_str(&raw).context("read legacy round result")?;
        round["legacyResultBody"] = json!(raw);
    }
    if round["state"] == "completed" {
        ensure!(
            round["result"].is_object() && round["completed"].is_number(),
            "completed legacy round lacks durable result or time"
        );
        let result = &round["result"];
        ensure!(
            result["report"].is_string()
                && result["report"] == round["report"]
                && result
                    .get("round")
                    .is_none_or(|value| value == &round["round"])
                && result
                    .get("attempt")
                    .is_none_or(|value| value == &round["attempt"]),
            "completed legacy result conflicts with the round identity"
        );
    }
    Ok(round)
}

fn assignments(
    dispatches: Vec<Value>,
    rounds: Vec<Value>,
    audit: Vec<Value>,
) -> Result<BTreeMap<String, Value>> {
    let mut records = BTreeMap::new();
    for mut row in dispatches {
        let id = text(&row, "dispatch")?.to_owned();
        ensure!(valid_id(&id), "invalid legacy dispatch {id}");
        ensure!(
            host::clean(text(&row, "scope")?) == row["scope"].as_str().unwrap(),
            "unsupported legacy scope for {id}"
        );
        text(&row, "role")?;
        ensure!(
            row["round"].as_u64().is_some_and(|n| n > 0)
                && valid_identity(text(&row, "attempt")?, "a-"),
            "invalid current legacy attempt for {id}"
        );
        let object = row.as_object_mut().context("invalid legacy dispatch row")?;
        let legacy_host = object.remove("host");
        let legacy_backend = object.remove("backend");
        row["legacyArchived"] = json!(true);
        row["legacyHost"] = json!(legacy_host);
        row["legacyBackend"] = json!(legacy_backend);
        row["rounds"] = json!([]);
        row["legacyAudit"] = json!([]);
        row["legacyResolutions"] = json!([]);
        row["watchdogFaults"] = json!([]);
        row["messages"] = json!([]);
        ensure!(
            records.insert(id, row).is_none(),
            "duplicate legacy dispatch"
        );
    }
    let mut attempts = BTreeSet::new();
    for round in rounds {
        let id = text(&round, "dispatch")?;
        let number = round["round"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid legacy round")?;
        let attempt = text(&round, "attempt")?;
        ensure!(
            valid_identity(attempt, "a-")
                && attempts.insert((id.to_owned(), number, attempt.to_owned())),
            "invalid or duplicate legacy round for {id}"
        );
        let row = records
            .get_mut(id)
            .with_context(|| format!("legacy round has no dispatch {id}"))?;
        row["rounds"]
            .as_array_mut()
            .unwrap()
            .push(convert_round(round)?);
    }
    for event in audit {
        let id = text(&event, "dispatch")?;
        let row = records
            .get_mut(id)
            .with_context(|| format!("legacy audit has no dispatch {id}"))?;
        row["legacyAudit"].as_array_mut().unwrap().push(event);
    }
    for (id, row) in &records {
        let current = row["rounds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|round| round["round"] == row["round"] && round["attempt"] == row["attempt"])
            .with_context(|| format!("legacy dispatch has no current round {id}"))?;
        ensure!(
            current["report"] == row["report"],
            "legacy current report conflicts with dispatch {id}"
        );
    }
    Ok(records)
}

fn matching_round<'a>(row: &'a Value, body: &Value, report: Option<&str>) -> Result<&'a Value> {
    let mut matching = row["rounds"].as_array().unwrap().iter().filter(|round| {
        body.get("round")
            .is_none_or(|number| number == &round["round"])
            && body
                .get("attempt")
                .is_none_or(|attempt| attempt == &round["attempt"])
            && report.is_none_or(|report| round["report"].as_str() == Some(report))
    });
    let round = matching
        .next()
        .context("receipt cannot be bound to a legacy round")?;
    ensure!(
        matching.next().is_none(),
        "receipt matches more than one legacy round"
    );
    Ok(round)
}

fn summary(record: &Import, duplicate: bool) -> Value {
    json!({"operation":"migrate-legacy","result":"completed","state":record.state,
        "sourceSha256":record.source_sha256,"stopProofSha256":record.stop_proof_sha256,
        "messageHighWater":record.message_high_water,"dispatches":record.dispatch_count,
        "messages":record.message_count,"owners":record.owners,
        "backup":record.state.join(ARCHIVE).join("router.db"),"duplicate":duplicate})
}

fn prior(target: &Path, source: &Path, sha: &str, proof: &str) -> Result<Option<Value>> {
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "import target is not a new directory");
            let record = published(target)?
                .context("import target already exists. Choose a new WFC_STATE")?;
            ensure!(
                record.source == source
                    && record.source_sha256 == sha
                    && record.stop_proof_sha256 == proof,
                "import target belongs to another source or stop proof"
            );
            ensure!(
                fingerprint(&target.join(ARCHIVE), None)? == sha,
                "published legacy backup fingerprint changed"
            );
            File::open(target.parent().context("missing import parent")?)?.sync_all()?;
            Ok(Some(summary(&record, true)))
        }
    }
}

fn rename_noreplace(staged: &Path, parent: &File, name: &OsStr) -> std::io::Result<()> {
    let staged = CString::new(staged.as_os_str().as_bytes())?;
    let name = CString::new(name.as_bytes())?;
    // The two directories are on the same filesystem. Do not replace even an
    // empty target created by a concurrent regular command or another importer.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            staged.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn run(args: &[OsString]) -> Result<Value> {
    let [source, proof] = messages::options(args, ["--offline-copy", "--stop-proof"], None)?;
    let source = Path::new(source.context("missing --offline-copy")?).canonicalize()?;
    ensure!(source.is_dir(), "--offline-copy must name a directory");
    let (parent, name) = parent_path()?;
    let target = parent.join(&name);
    ensure!(
        !target.starts_with(&source) && !source.starts_with(&target),
        "source and import target must not overlap"
    );
    let source_meta = fs::metadata(&source)?;
    for ancestor in parent.ancestors() {
        let metadata = fs::metadata(ancestor)?;
        ensure!(
            metadata.dev() != source_meta.dev() || metadata.ino() != source_meta.ino(),
            "source and import target must not overlap"
        );
    }
    let parent_file = File::open(&parent)?;
    let proof_bytes = fs::read(proof.context("missing --stop-proof")?)?;
    let proof_sha = format!("{:x}", Sha256::digest(&proof_bytes));
    let proof: StopProof = serde_json::from_slice(&proof_bytes).context("read stop proof")?;
    ensure!(
        proof.target == target,
        "stop proof target differs from the selected WFC_STATE"
    );
    ensure!(
        proof.writers_stopped
            && sha256_text(&proof.source_sha256)
            && !proof.stopped_by.trim().is_empty()
            && !proof.evidence.trim().is_empty(),
        "stop proof needs writersStopped=true, sourceSha256, stoppedBy and evidence"
    );
    if let Some(receipt) = prior(&target, &source, &proof.source_sha256, &proof_sha)? {
        ensure!(
            fingerprint(&source, None)? == proof.source_sha256,
            "offline source does not match stop-proof SHA-256"
        );
        return Ok(receipt);
    }
    super::ensure_state_writable(&target)?;
    let scratch = wfc_legacy_state::import_scratch(&parent)?;
    let staged = scratch.path().join("state");
    private_dir(&staged)?;
    // Bind the marker to the final path before staging any writable state.
    // The ordinary writer checks it even if this process is interrupted.
    let mut import = Import {
        version: 1,
        state: target.clone(),
        source: source.clone(),
        source_sha256: proof.source_sha256,
        stop_proof_sha256: proof_sha,
        stopped_by: proof.stopped_by,
        evidence: proof.evidence,
        message_high_water: 0,
        dispatch_count: 0,
        message_count: 0,
        owners: Vec::new(),
    };
    write_json(&staged.join(MARKER), &serde_json::to_value(&import)?, None)?;
    let archive = staged.join(ARCHIVE);
    private_dir(&archive)?;
    let database = archive.join("router.db");
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&database)?;
    ensure!(
        fingerprint(&source, Some(&mut backup))? == import.source_sha256,
        "offline source does not match stop-proof SHA-256"
    );
    backup.sync_all()?;
    drop(backup);
    let ledger = stopped_ledger(&database)?;
    let mut records = assignments(ledger.dispatches, ledger.rounds, ledger.audit)?;
    for (rows, key) in [
        (ledger.faults, "fault"),
        (ledger.host_errors, "hostError"),
        (ledger.screen_watches, "screenObservedSince"),
        (ledger.watchdog_faults, "watchdogFaults"),
    ] {
        for mut item in rows {
            let dispatch = text(&item, "dispatch")?;
            let record = records
                .get_mut(dispatch)
                .with_context(|| format!("legacy {key} has no dispatch {dispatch}"))?;
            if key == "watchdogFaults" {
                ensure!(
                    item["round"].is_u64() && item["attempt"].is_string(),
                    "legacy watchdog fault lacks attempt identity"
                );
                matching_round(record, &item, None)
                    .context("unverified legacy watchdog fault attempt")?;
                item.as_object_mut().unwrap().remove("dispatch");
                record[key].as_array_mut().unwrap().push(item);
            } else if key == "fault" {
                item.as_object_mut().unwrap().remove("dispatch");
                record[key] = item;
            } else {
                record[key] = item[key].clone();
            }
        }
    }
    let converted = history::receipts(&mut records, &ledger.messages, ledger.resolutions)?;
    let dispatch_dir = staged.join("dispatches");
    private_dir(&dispatch_dir)?;
    for (id, record) in &records {
        write_json(&dispatch_dir.join(format!("{id}.json")), record, None)?;
    }
    messages::stage_legacy_messages(&staged, &converted)?;
    import.owners =
        controller::stage_legacy_owners(&staged, ledger.owners, proof.owner_replacements)?;
    import.message_high_water = ledger.high_water;
    import.dispatch_count = records.len();
    import.message_count = ledger.messages.len();
    write_json(&staged.join(MARKER), &serde_json::to_value(&import)?, None)?;
    for directory in [&archive, &dispatch_dir, &staged] {
        File::open(directory)?.sync_all()?;
    }
    File::open(scratch.path())?.sync_all()?;
    ensure!(
        fingerprint(&source, None)? == import.source_sha256,
        "offline source changed before publishing import"
    );
    ensure!(
        fingerprint(&archive, None)? == import.source_sha256,
        "staged legacy backup fingerprint changed"
    );
    match rename_noreplace(&staged, &parent_file, &name) {
        Ok(()) => parent_file.sync_all()?,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return prior(
                &target,
                &source,
                &import.source_sha256,
                &import.stop_proof_sha256,
            )?
            .context("import target changed during publication");
        }
        Err(error) => return Err(error).context("publish complete legacy import"),
    }
    Ok(summary(&import, false))
}
