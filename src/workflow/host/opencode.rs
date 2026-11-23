//! Proof of the OpenCode session that accepted an original worker launch.
#[cfg(test)]
mod tests;

use anyhow::{ensure, Context, Result};
use rusqlite::{functions::FunctionFlags, params, types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env, fs,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Opening {
    dispatch: String,
    attempt: String,
    cwd: PathBuf,
    device: u64,
    inode: u64,
    database: PathBuf,
    brief_sha256: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Proof {
    storage_source: StorageSource,
    adopted_attempt: String,
    current_attempt: String,
    session: String,
    project: String,
    first_message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum StorageSource {
    Legacy,
    SessionV2,
    Unified,
}

fn effective_setting(launch: &Map<String, Value>, key: &str) -> Option<PathBuf> {
    launch
        .get(key)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| env::var_os(key).map(PathBuf::from))
        .filter(|path| !path.as_os_str().is_empty())
}

fn selected_database(launch: &Map<String, Value>) -> Result<Option<PathBuf>> {
    let host = effective_setting(launch, "OPENCODE_DB");
    let lookup = effective_setting(launch, "WFC_OPENCODE_DB");
    if let Some(path) = &lookup {
        ensure!(
            path.is_absolute(),
            "WFC_OPENCODE_DB must be an absolute path"
        );
        ensure!(
            host.as_ref().is_none_or(|host| host == path),
            "OPENCODE_DB and WFC_OPENCODE_DB identify different stores"
        );
    }
    Ok(host.or(lookup))
}

fn launch_directory(state: &Path, dispatch: &str, attempt: &str) -> Result<PathBuf> {
    ensure!(
        super::super::valid_id(dispatch) && super::super::valid_identity(attempt, "a-"),
        "invalid OpenCode launch identity"
    );
    Ok(state
        .canonicalize()?
        .join(format!("opencode-{dispatch}-{attempt}")))
}

fn store_directory(path: &Path) -> Result<fs::Metadata> {
    let info = fs::symlink_metadata(path).context("missing private OpenCode launch directory")?;
    ensure!(
        info.is_dir() && info.mode() & 0o777 == 0o700 && info.uid() == unsafe { libc::geteuid() },
        "OpenCode launch directory is not private"
    );
    Ok(info)
}

fn verify_opening(state: &Path, record: &Value) -> Result<Opening> {
    let opening = Opening::deserialize(&record["opencodeOpening"])
        .context("missing private OpenCode opening evidence")?;
    let directory = launch_directory(state, &opening.dispatch, &opening.attempt)?;
    ensure!(
        record["dispatch"] == opening.dispatch
            && record["cwd"]
                .as_str()
                .is_some_and(|cwd| Path::new(cwd) == opening.cwd)
            && opening.cwd.is_absolute()
            && opening.database == directory.join("opencode.db")
            && opening.brief_sha256.len() == 64
            && opening.brief_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "OpenCode opening does not match the recorded launch"
    );
    let info = store_directory(&directory)?;
    ensure!(
        (info.dev(), info.ino()) == (opening.device, opening.inode),
        "private OpenCode launch directory changed"
    );
    Ok(opening)
}

// Only a caller that authenticated the original host's adoption callback may
// supply this receipt. Database rows and the rendered screen cannot issue it.
pub(crate) struct OpencodeAdoption<'a> {
    pub dispatch: &'a str,
    pub attempt: &'a str,
    pub cwd: &'a Path,
    pub database: &'a Path,
    pub session: &'a str,
    pub first_message: &'a str,
}

#[allow(dead_code)] // Wired by the shared resume transaction after host adoption is verified.
pub(crate) fn adopt_opencode_session(
    state: &Path,
    record: &mut Value,
    receipt: OpencodeAdoption<'_>,
) -> Result<()> {
    let opening = verify_opening(state, record)?;
    ensure!(
        record["host"] == "opencode"
            && record["backend"] == "tmux"
            && record["round"] == 1
            && record["attempt"] == opening.attempt
            && receipt.dispatch == opening.dispatch
            && receipt.attempt == opening.attempt
            && receipt.cwd == opening.cwd
            && receipt.database == opening.database,
        "OpenCode adoption receipt belongs to a different launch"
    );
    let previous = if record["hostSession"].is_null() && record["opencodeSession"].is_null() {
        None
    } else {
        let proof = saved_proof(record, &opening)?;
        ensure!(
            proof.session == receipt.session && proof.first_message == receipt.first_message,
            "OpenCode adoption conflicts with the recorded session"
        );
        Some(proof)
    };
    let proof = verify_session(
        &opening,
        receipt.attempt,
        receipt.session,
        receipt.first_message,
        previous.as_ref().map(|proof| proof.storage_source),
    )?;
    ensure!(
        previous.as_ref().is_none_or(|previous| previous == &proof),
        "recorded OpenCode session proof changed"
    );
    let proof = serde_json::to_value(proof)?;
    record["hostSession"] = json!(receipt.session);
    record["opencodeSession"] = proof;
    Ok(())
}

fn saved_proof(record: &Value, opening: &Opening) -> Result<Proof> {
    let proof = Proof::deserialize(&record["opencodeSession"])
        .context("missing verified original OpenCode adoption receipt or storage source")?;
    ensure!(
        record["hostSession"] == proof.session
            && proof.adopted_attempt == opening.attempt
            && record["attempt"] == proof.current_attempt,
        "recorded OpenCode adoption conflicts with the launch"
    );
    Ok(proof)
}

pub(super) fn prepare(
    state: &Path,
    launch: &mut Map<String, Value>,
    record: &mut Value,
    original: Option<&Value>,
    brief: &str,
) -> Result<Option<String>> {
    let configured = selected_database(launch)?;
    if let Some(original) = original {
        let opening = verify_opening(state, original)?;
        ensure!(
            configured
                .as_ref()
                .is_none_or(|path| path == &opening.database),
            "configured OpenCode store differs from the original dispatch"
        );
        let mut previous = saved_proof(original, &opening)?;
        ensure!(
            verify_session(
                &opening,
                &previous.current_attempt,
                &previous.session,
                &previous.first_message,
                Some(previous.storage_source),
            )? == previous,
            "recorded OpenCode session proof changed"
        );
        launch.insert("OPENCODE_DB".into(), json!(opening.database));
        for key in ["opencodeOpening", "hostSession"] {
            record[key] = original[key].clone();
        }
        previous.current_attempt = record["attempt"]
            .as_str()
            .context("missing OpenCode resume attempt")?
            .into();
        record["opencodeSession"] = serde_json::to_value(&previous)?;
        return Ok(Some(previous.session));
    }

    let isolated = record["backend"] == "tmux"
        && effective_setting(launch, "WFC_OPENCODE_ISOLATED_STORE")
            .is_some_and(|value| value == Path::new("1"));
    if isolated {
        ensure!(
            configured.is_none(),
            "private OpenCode launch cannot use a configured database"
        );
        let dispatch = record["dispatch"].as_str().context("missing dispatch")?;
        let attempt = record["attempt"].as_str().context("missing attempt")?;
        let cwd = PathBuf::from(record["cwd"].as_str().context("missing cwd")?);
        ensure!(cwd.is_absolute(), "invalid OpenCode launch directory");
        let directory = launch_directory(state, dispatch, attempt)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context("cannot reserve a fresh private OpenCode launch directory")?;
        let info = store_directory(&directory)?;
        let database = directory.join("opencode.db");
        launch.insert(
            "OPENCODE_DB".into(),
            json!(database
                .to_str()
                .context("invalid OpenCode database path")?),
        );
        record["opencodeOpening"] = serde_json::to_value(Opening {
            dispatch: dispatch.into(),
            attempt: attempt.into(),
            cwd,
            device: info.dev(),
            inode: info.ino(),
            database,
            brief_sha256: format!("{:x}", Sha256::digest(brief.as_bytes())),
        })?;
    } else if let Some(database) = configured.filter(|path| path.is_absolute()) {
        // Keep explicit first-launch storage selection without claiming provenance.
        launch.insert(
            "OPENCODE_DB".into(),
            json!(database
                .to_str()
                .context("invalid OpenCode database path")?),
        );
    }
    Ok(None)
}

fn path_contains(directory: &Path, cwd: &Path) -> bool {
    directory.is_absolute()
        && !directory
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && cwd.starts_with(directory)
}

fn in_project(
    db: &Connection,
    tables: &HashSet<String>,
    project: &str,
    worktree: &Path,
    cwd: &Path,
) -> Result<bool> {
    if project == "global" && worktree == Path::new("/") {
        return Ok(true);
    }
    if worktree != Path::new("/") && path_contains(worktree, cwd) {
        return Ok(true);
    }
    for table in ["worktree", "project_directory"] {
        if tables.contains(table) {
            let mut query = db.prepare(&format!(
                "SELECT directory FROM {table} WHERE project_id=?1"
            ))?;
            let rows = query.query_map([project], |row| row.get::<_, String>(0))?;
            for path in rows {
                let path = path?;
                let directory = Path::new(&path);
                if directory != Path::new("/") && path_contains(directory, cwd) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

// Current builds use session_message with session, while earlier v2 uses
// session_v2. The foreign key gives the association even when tables coexist.
fn modern_source(db: &Connection, tables: &HashSet<String>) -> Result<Option<StorageSource>> {
    if !tables.contains("session_message") {
        return Ok(None);
    }
    let mut foreign = db.prepare(
        "SELECT [table] FROM pragma_foreign_key_list('session_message')
         WHERE [from]='session_id' AND [to]='id'",
    )?;
    let parents = foreign
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match parents.as_slice() {
        [table] if table == "session_v2" && tables.contains(table) => {
            Ok(Some(StorageSource::SessionV2))
        }
        [table] if table == "session" && tables.contains(table) => Ok(Some(StorageSource::Unified)),
        _ => anyhow::bail!("unrecognized OpenCode session_message association"),
    }
}

fn select_source(db: &Connection, tables: &HashSet<String>, id: &str) -> Result<StorageSource> {
    let modern = modern_source(db, tables)?;
    let legacy = ["session", "message", "part"]
        .iter()
        .all(|table| tables.contains(*table));
    let exists = |sql: &str| db.query_row(sql, [id], |row| row.get::<_, bool>(0));
    // Independent session tables can retain conflicting identity claims even
    // when the formal message table is missing or now points elsewhere.
    let historical_root = tables.contains("session_v2")
        && exists("SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id=?1)")?;
    ensure!(
        !historical_root
            || !tables.contains("session")
            || !exists("SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)")?,
        "ambiguous OpenCode session ID in historical and legacy session tables"
    );
    match modern {
        Some(StorageSource::Unified) => {
            // The unified session row can have both a formal message and an old
            // projection. Any formal user entry takes precedence at adoption.
            if exists(
                "SELECT EXISTS(SELECT 1 FROM session_message WHERE session_id=?1 AND type='user')",
            )? || !legacy
            {
                return Ok(StorageSource::Unified);
            }
        }
        Some(StorageSource::SessionV2) => {
            if historical_root || !legacy {
                return Ok(StorageSource::SessionV2);
            }
        }
        _ => (),
    }
    ensure!(legacy, "unrecognized OpenCode session storage schema");
    Ok(StorageSource::Legacy)
}

fn session_query(source: StorageSource) -> String {
    let (parent, root) = match source {
        StorageSource::SessionV2 => ("session_v2", "AND s.fork_session_id IS NULL"),
        StorageSource::Unified => ("session", ""),
        StorageSource::Legacy => return
            "SELECT s.id,m.id,s.project_id,p.worktree FROM session s
             LEFT JOIN project p ON p.id=s.project_id
             JOIN message m ON m.session_id=s.id AND m.id=(
                SELECT first.id FROM message first WHERE first.session_id=s.id AND json_extract(first.data,'$.role')='user'
                ORDER BY first.time_created,first.id LIMIT 1)
             WHERE s.directory=?1 AND s.parent_id IS NULL AND s.id=?3
             AND ?2=(SELECT CASE WHEN COUNT(*)=1 THEN MAX(wfc_opening_digest(json_extract(t.data,'$.text'))) END
                FROM part t WHERE t.message_id=m.id AND t.session_id=s.id
                AND json_extract(t.data,'$.type')='text' AND json_type(t.data,'$.text')='text'
                AND COALESCE(json_extract(t.data,'$.synthetic'),0)=0 AND COALESCE(json_extract(t.data,'$.ignored'),0)=0)"
            .into(),
    };
    format!(
        "SELECT s.id,m.id,s.project_id,p.worktree FROM {parent} s
         LEFT JOIN project p ON p.id=s.project_id
         JOIN session_message m ON m.session_id=s.id AND m.type='user'
         WHERE s.directory=?1 AND s.parent_id IS NULL AND s.id=?3 {root}
         AND m.seq=(SELECT MIN(first.seq) FROM session_message first WHERE first.session_id=s.id AND first.type='user')
         AND (SELECT COUNT(*) FROM session_message first WHERE first.session_id=s.id AND first.seq=m.seq)=1
         AND json_type(m.data,'$.text')='text' AND wfc_opening_digest(json_extract(m.data,'$.text'))=?2"
    )
}

fn verify_session(
    opening: &Opening,
    current_attempt: &str,
    id: &str,
    first_message: &str,
    expected_source: Option<StorageSource>,
) -> Result<Proof> {
    ensure!(
        id.strip_prefix("ses_").is_some_and(|s| !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
            && !first_message.is_empty(),
        "invalid OpenCode adoption identity"
    );
    let cwd = opening
        .cwd
        .to_str()
        .context("invalid OpenCode launch directory")?;
    ensure!(
        fs::symlink_metadata(&opening.database).is_ok_and(|info| info.is_file()),
        "the recorded OpenCode store is missing or inaccessible"
    );
    let mut db = Connection::open_with_flags(
        &opening.database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("cannot open the recorded OpenCode store read-only")?;
    db.busy_timeout(Duration::from_millis(250))?;
    db.execute_batch("PRAGMA trusted_schema=OFF")?;
    db.create_scalar_function(
        "wfc_opening_digest",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_DIRECTONLY,
        |context| {
            Ok(match context.get_raw(0) {
                ValueRef::Text(text) => Some(format!("{:x}", Sha256::digest(text))),
                _ => None,
            })
        },
    )?;
    let db = db.transaction()?;
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let source = select_source(&db, &tables, id)?;
    ensure!(
        expected_source.is_none_or(|expected| expected == source),
        "recorded OpenCode storage source changed"
    );
    let mut query = db
        .prepare(&session_query(source))
        .context("cannot read OpenCode session schema")?;
    let mut results = query.query(params![cwd, opening.brief_sha256, id])?;
    let mut matched = None;
    while let Some(row) = results
        .next()
        .context("cannot read OpenCode session evidence")?
    {
        let proof = Proof {
            storage_source: source,
            adopted_attempt: opening.attempt.clone(),
            current_attempt: current_attempt.into(),
            session: row.get(0)?,
            first_message: row.get(1)?,
            project: row.get(2)?,
        };
        let worktree: Option<String> = row.get(3)?;
        ensure!(
            proof.session == id
                && proof.first_message == first_message
                && !proof.project.is_empty(),
            "invalid OpenCode session identity"
        );
        ensure!(
            in_project(
                &db,
                &tables,
                &proof.project,
                Path::new(
                    worktree
                        .as_deref()
                        .context("OpenCode session has no project record")?
                ),
                Path::new(cwd)
            )?,
            "matching OpenCode session belongs to a different project"
        );
        ensure!(
            matched.as_ref().is_none_or(|previous| *previous == proof),
            "conflicting OpenCode session proof"
        );
        matched = Some(proof);
    }
    matched.context("adopted OpenCode session does not match the original first user message")
}
