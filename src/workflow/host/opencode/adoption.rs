//! First-launch receipt for OpenCode 1.19.5's v1 TUI prompt path.
use super::super::super::{assignment, controller, decode_record, messages, write_json};
use super::{adopt_receipt, store_directory, verify_opening, OpencodeAdoption, StorageSource};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    env,
    ffi::OsString,
    fs::{self, Metadata, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const PLUGIN: &str = "wfc-adoption.mjs";
const PLUGIN_SOURCE: &[u8] = include_bytes!("adoption.mjs");
const CAPABILITY: &str = "wfc-adoption.capability";
const BINDING: &str = "wfc-adoption.sha256";
const MAX_RECEIPT: u64 = 16 * 1024;

fn require_private_state(state: &Path) -> Result<()> {
    store_directory(state).context("OpenCode adoption requires a private WFC_STATE")?;
    ensure!(state.canonicalize()? == state, "WFC_STATE is not canonical");
    Ok(())
}

fn records_dir(state: &Path) -> Result<PathBuf> {
    let dir = state.join("dispatches");
    let info = fs::symlink_metadata(&dir)?;
    ensure!(
        info.is_dir() && info.mode() & 0o022 == 0 && info.uid() == unsafe { libc::geteuid() },
        "unsafe OpenCode assignment directory"
    );
    Ok(dir)
}

pub(super) fn prepare_state(state: &Path) -> Result<()> {
    let mut directory = fs::DirBuilder::new();
    directory.mode(0o700).recursive(true).create(state)?;
    require_private_state(state)?;
    directory.create(state.join("dispatches"))?;
    records_dir(state)?;
    Ok(())
}

pub(in crate::workflow::host) fn configure(
    state: &Path,
    launch: &Map<String, Value>,
    record: &Value,
    config: &mut Value,
) -> Result<()> {
    let opening = verify_opening(state, record)?;
    let key = "OPENCODE_CONFIG_CONTENT";
    let existing = launch
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| env::var(key).ok())
        .filter(|content| !content.is_empty());
    if let Some(existing) = existing {
        // The controlled inline overlay accepts JSON. OpenCode still loads
        // its regular config sources and merges their plugin origins.
        let mut supplied: Map<String, Value> = serde_json::from_str(&existing)
            .map_err(|_| anyhow::anyhow!("invalid inline OpenCode configuration"))?;
        supplied
            .entry("permission")
            .or_insert_with(|| config["permission"].take());
        *config = Value::Object(supplied);
    }
    let plugins = config
        .as_object_mut()
        .unwrap()
        .entry("plugin")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("inline OpenCode plugin configuration must be an array")?;
    let directory = opening
        .database
        .parent()
        .context("missing OpenCode store directory")?;
    let path = directory.join(PLUGIN);
    let capability = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    write_private(&path, PLUGIN_SOURCE)?;
    write_private(
        &directory.join(BINDING),
        messages::body_hash(&capability).as_bytes(),
    )?;
    write_private(&directory.join(CAPABILITY), capability.as_bytes())?;
    messages::sync_directory(directory)?;
    plugins.push(json!([path, {"briefSha256": opening.brief_sha256, "cwd": opening.cwd}]));
    Ok(())
}

fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    dispatch: String,
    attempt: String,
    cwd: PathBuf,
    database: PathBuf,
    session: String,
    first_message: String,
}

fn receive() -> Result<()> {
    let state = PathBuf::from(env::var_os("WFC_STATE").context("missing state")?);
    let secret = controller::worker_callback().context("missing worker capability")?;
    let capability = env::var("WFC_OPENCODE_ADOPTION")?;
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_RECEIPT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECEIPT,
        "OpenCode receipt exceeds size limit"
    );
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        env::var("WFC_DISPATCH").ok().as_deref() == Some(receipt.dispatch.as_str())
            && env::var("WFC_ATTEMPT").ok().as_deref() == Some(receipt.attempt.as_str())
            && env::var("WFC_ROUND").ok().as_deref() == Some("1")
            && env::var_os("OPENCODE_DB").is_some_and(|path| Path::new(&path) == receipt.database)
            && env::current_dir()? == receipt.cwd,
        "OpenCode callback environment changed"
    );
    publish(
        &state,
        &receipt,
        secret.to_str().context("invalid worker capability")?,
        &capability,
    )
}

pub(crate) fn first_message() -> bool {
    // A hook invocation reports only success or failure to its parent.
    receive().is_ok()
}

fn check_file(path: &Path, check: impl FnOnce(&Metadata) -> bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(info)
            if info.is_file()
                && info.nlink() == 1
                && info.uid() == unsafe { libc::geteuid() }
                && check(&info) =>
        {
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        _ => anyhow::bail!("unsafe OpenCode adoption file"),
    }
}

fn publish(state: &Path, receipt: &Receipt, secret: &str, capability: &str) -> Result<()> {
    use super::super::super as workflow;
    ensure!(
        workflow::valid_id(&receipt.dispatch)
            && workflow::valid_identity(&receipt.attempt, "a-")
            && receipt
                .first_message
                .strip_prefix("msg_")
                .is_some_and(|id| !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')),
        "invalid OpenCode callback identity"
    );
    require_private_state(state)?;
    ensure!(
        capability.len() == 64 && capability.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid OpenCode plugin capability"
    );
    let _ledger = messages::lock_for_private_publish(state)?;
    let dir = records_dir(state)?;
    for extension in ["json", "pending"] {
        check_file(
            &dir.join(format!("{}.{extension}", receipt.dispatch)),
            |info| info.mode() & 0o077 == 0,
        )?;
    }
    let (file, path) = assignment::open_assignment(&dir, &receipt.dispatch)?;
    let mut row = decode_record(file, &path)?;
    ensure!(
        !secret.is_empty()
            && row[controller::CALLBACK_HASH] == messages::body_hash(secret)
            && row["dispatch"] == receipt.dispatch
            && row["attempt"] == receipt.attempt
            && row["executionId"] == receipt.attempt,
        "invalid OpenCode attempt capability"
    );
    let opening = verify_opening(state, &row)?;
    ensure!(
        opening.database == receipt.database && opening.cwd == receipt.cwd,
        "OpenCode receipt targets a different launch"
    );
    let plugin = opening.database.with_file_name(PLUGIN);
    check_file(&plugin, |info| {
        info.mode() & 0o777 == 0o600 && info.len() == PLUGIN_SOURCE.len() as u64
    })?;
    ensure!(
        fs::read(plugin)? == PLUGIN_SOURCE,
        "missing controlled OpenCode plugin"
    );
    let binding = opening.database.with_file_name(BINDING);
    check_file(&binding, |info| {
        info.mode() & 0o777 == 0o600 && info.len() == 64
    })?;
    ensure!(
        fs::read(binding)? == messages::body_hash(capability).as_bytes(),
        "invalid OpenCode plugin capability"
    );
    for suffix in ["", "-wal", "-shm"] {
        let mut file = OsString::from(opening.database.as_os_str());
        file.push(suffix);
        check_file(Path::new(&file), |_| true)?;
    }
    let previous = row["opencodeSession"].clone();
    adopt_receipt(
        state,
        &mut row,
        OpencodeAdoption {
            dispatch: &receipt.dispatch,
            attempt: &receipt.attempt,
            cwd: &receipt.cwd,
            database: &receipt.database,
            session: &receipt.session,
            first_message: &receipt.first_message,
        },
        // This hook is paired with the v1 message/part projector. A same-ID
        // record from the independent v2 source cannot authenticate its event.
        Some(StorageSource::Legacy),
    )?;
    if previous != row["opencodeSession"] {
        write_json(&path, &row, None)?;
    }
    messages::sync_directory(&dir)
}

/// Reload the reservation with messages.lock held before settling tmux create.
pub(in crate::workflow) fn preserve_pending(path: &Path, row: &mut Value) -> Result<()> {
    let mut pending = decode_record(fs::File::open(path)?, path)?;
    let adopted = pending["opencodeSession"].take();
    let session = pending["hostSession"].take();
    let pending = pending.as_object_mut().context("invalid reservation")?;
    pending.remove("opencodeSession");
    pending.remove("hostSession");
    ensure!(
        *row == Value::Object(std::mem::take(pending)),
        "OpenCode reservation changed"
    );
    if !adopted.is_null() {
        row["opencodeSession"] = adopted;
        row["hostSession"] = session;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
