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
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Opening {
    attempt: String,
    database: PathBuf,
    brief_sha256: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Proof {
    project: String,
    first_message: String,
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

pub(super) fn prepare(
    launch: &mut Map<String, Value>,
    record: &mut Value,
    original: Option<&Value>,
    brief: &str,
) -> Result<Option<String>> {
    let configured = selected_database(launch)?;
    let Some(original) = original else {
        if let Some(database) = configured.filter(|path| path.is_absolute()) {
            // Pass the recorded store explicitly even through an older tmux server.
            launch.insert(
                "OPENCODE_DB".into(),
                json!(database
                    .to_str()
                    .context("invalid OpenCode database path")?),
            );
            record["opencodeOpening"] = serde_json::to_value(Opening {
                attempt: record["attempt"]
                    .as_str()
                    .context("missing attempt")?
                    .into(),
                database,
                brief_sha256: format!("{:x}", Sha256::digest(brief.as_bytes())),
            })?;
        }
        return Ok(None);
    };

    let opening: Opening = Opening::deserialize(&original["opencodeOpening"])
        .context("missing recorded OpenCode launch evidence")?;
    ensure!(
        opening.database.is_absolute(),
        "invalid recorded OpenCode store"
    );
    ensure!(
        configured
            .as_ref()
            .is_none_or(|path| path == &opening.database),
        "configured OpenCode store differs from the original dispatch"
    );
    ensure!(
        super::super::valid_identity(&opening.attempt, "a-")
            && opening.brief_sha256.len() == 64
            && opening.brief_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid OpenCode opening evidence"
    );
    let saved = if original["hostSession"].is_null() && original["opencodeSession"].is_null() {
        ensure!(
            original["attempt"] == opening.attempt && original["round"] == 1,
            "OpenCode opening evidence belongs to another attempt"
        );
        None
    } else {
        Some((
            original["hostSession"]
                .as_str()
                .context("missing recorded OpenCode session ID")?,
            Proof::deserialize(&original["opencodeSession"])
                .context("invalid recorded OpenCode session proof")?,
        ))
    };
    let cwd = original["cwd"]
        .as_str()
        .context("missing original OpenCode directory")?;
    let (id, proof) = lookup(&opening, cwd)?;
    if let Some((previous_id, previous_proof)) = saved {
        ensure!(
            previous_id == id && previous_proof == proof,
            "recorded OpenCode session proof changed"
        );
    }
    launch.insert("OPENCODE_DB".into(), json!(opening.database));
    record["opencodeOpening"] = original["opencodeOpening"].clone();
    record["hostSession"] = json!(id);
    record["opencodeSession"] = serde_json::to_value(proof)?;
    Ok(Some(id))
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
fn modern_query(
    db: &Connection,
    tables: &HashSet<String>,
) -> Result<Option<(&'static str, String)>> {
    if !tables.contains("session_message") {
        return Ok(None);
    }
    let mut foreign = db.prepare(
        "SELECT [table] FROM pragma_foreign_key_list('session_message')\n\
        WHERE [from]='session_id' AND [to]='id'",
    )?;
    let parents = foreign
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (parent, root) = match parents.as_slice() {
        [table] if table == "session_v2" && tables.contains(table) => {
            ("session_v2", "AND s.fork_session_id IS NULL")
        }
        [table] if table == "session" && tables.contains(table) => ("session", ""),
        _ => anyhow::bail!("unrecognized OpenCode session_message association"),
    };
    Ok(Some((parent, format!(
        "SELECT s.id,m.id,s.project_id,p.worktree FROM {parent} s
         LEFT JOIN project p ON p.id=s.project_id
         JOIN session_message m ON m.session_id=s.id AND m.type='user'
         WHERE s.directory=?1 AND s.parent_id IS NULL {root}
         AND m.seq=(SELECT MIN(first.seq) FROM session_message first WHERE first.session_id=s.id AND first.type='user')
         AND (SELECT COUNT(*) FROM session_message first WHERE first.session_id=s.id AND first.seq=m.seq)=1
         AND json_type(m.data,'$.text')='text' AND wfc_opening_digest(json_extract(m.data,'$.text'))=?2"
    ))))
}

fn lookup(opening: &Opening, cwd: &str) -> Result<(String, Proof)> {
    ensure!(
        Path::new(cwd).is_absolute(),
        "invalid OpenCode launch directory"
    );
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
    let modern = modern_query(&db, &tables)?;
    let mut queries = Vec::new();
    if let Some((_, sql)) = &modern {
        queries.push(sql.clone());
    }
    if ["session", "message", "part"]
        .iter()
        .all(|table| tables.contains(*table))
    {
        let unmigrated = if modern
            .as_ref()
            .is_some_and(|(parent, _)| *parent == "session")
        {
            "AND NOT EXISTS (SELECT 1 FROM session_message n WHERE n.session_id=s.id AND n.type='user')"
        } else {
            ""
        };
        queries.push(format!(
            "SELECT s.id,m.id,s.project_id,p.worktree FROM session s
             LEFT JOIN project p ON p.id=s.project_id
             JOIN message m ON m.session_id=s.id AND m.id=(
                SELECT first.id FROM message first WHERE first.session_id=s.id AND json_extract(first.data,'$.role')='user'
                ORDER BY first.time_created,first.id LIMIT 1)
             WHERE s.directory=?1 AND s.parent_id IS NULL {unmigrated}
             AND ?2=(SELECT CASE WHEN COUNT(*)=1 THEN MAX(wfc_opening_digest(json_extract(t.data,'$.text'))) END
                FROM part t WHERE t.message_id=m.id AND t.session_id=s.id
                AND json_extract(t.data,'$.type')='text' AND json_type(t.data,'$.text')='text'
                AND COALESCE(json_extract(t.data,'$.synthetic'),0)=0 AND COALESCE(json_extract(t.data,'$.ignored'),0)=0)"
        ));
    }
    ensure!(
        !queries.is_empty(),
        "unrecognized OpenCode session storage schema"
    );
    let mut matched = None;
    for sql in queries {
        let mut query = db
            .prepare(&sql)
            .context("cannot read OpenCode session schema")?;
        let mut results = query.query(params![cwd, opening.brief_sha256])?;
        while let Some(row) = results
            .next()
            .context("cannot read OpenCode session evidence")?
        {
            let id: String = row.get(0)?;
            let proof = Proof {
                first_message: row.get(1)?,
                project: row.get(2)?,
            };
            let worktree: Option<String> = row.get(3)?;
            ensure!(
                id.strip_prefix("ses_").is_some_and(|s| !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
                    && !proof.first_message.is_empty()
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
            let candidate = (id, proof);
            ensure!(
                matched
                    .as_ref()
                    .is_none_or(|previous| *previous == candidate),
                "multiple root OpenCode sessions or first-message identities match this opening"
            );
            matched = Some(candidate);
        }
    }
    matched.context("no root OpenCode session matches this attempt's first user message")
}
