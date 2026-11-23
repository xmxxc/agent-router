use crate::{open, resolve, schema};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{
    functions::FunctionFlags, Connection, OpenFlags, OptionalExtension, TransactionBehavior,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

// CAST keeps SQLite's stored text encoding; asking SQLite for UTF-8 first
// can silently replace a malformed UTF-16 surrogate pair with a valid character.
const BODY_SHA: &str = "body_sha(typeof(body),CAST(body AS BLOB))";
const INCOMPLETE_RECEIPT: &str = "coalesce(message_uid,'')='' OR coalesce(body_hash,'')='' OR
    coalesce(state,'')='' OR coalesce(status_version,0)=0";
const DELIVERY_CONFLICT: &str =
    "(coalesce(delivered,0)=0 AND state='received') OR (delivered<>0 AND state='unsent')";
const IDENTITY: &str = "coalesce(backend,'')='' OR coalesce(scope,'')='' OR coalesce(role,'')=''
    OR coalesce(current_round,0)=0 OR coalesce(current_attempt,'')=''";
const NO_SNAPSHOT: &str = "NOT EXISTS (SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id)";

fn has_repairs(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM message WHERE {INCOMPLETE_RECEIPT} OR {DELIVERY_CONFLICT})
        OR EXISTS(SELECT 1 FROM dispatch WHERE coalesce(backend,'')='')"
        ),
        [],
        |r| r.get(0),
    )?)
}

fn backup(state: &Path) -> Result<PathBuf> {
    let temp = tempfile::Builder::new()
        .prefix(".router-backup-")
        .tempfile_in(state)?;
    let reader = open(&state.join("router.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    reader.backup("main", temp.path(), None)?;
    temp.as_file().sync_all()?;
    // Only a completed backup is published under the recovery filename.
    let path = state.join(format!("router.before-{}.db", Uuid::new_v4().simple()));
    temp.persist_noclobber(&path)?;
    #[cfg(unix)]
    fs::File::open(state)?.sync_all()?;
    Ok(path)
}

fn register_functions(db: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    db.create_scalar_function("new_uuid", 0, flags, |_| {
        Ok(Uuid::new_v4().simple().to_string())
    })?;
    let encoding: String = db.query_row("PRAGMA encoding", [], |r| r.get(0))?;
    db.create_scalar_function("body_sha", 2, flags, move |ctx| {
        if ctx.get_raw(0).as_str()? != "text" {
            return Ok(None);
        }
        let bytes = ctx.get_raw(1).as_bytes()?;
        let digest = match encoding.as_str() {
            "UTF-8" => std::str::from_utf8(bytes)
                .ok()
                .map(|_| Sha256::digest(bytes)),
            "UTF-16le" | "UTF-16be" if bytes.len().is_multiple_of(2) => {
                let units = bytes.chunks_exact(2).map(|pair| {
                    let unit = [pair[0], pair[1]];
                    if encoding == "UTF-16le" {
                        u16::from_le_bytes(unit)
                    } else {
                        u16::from_be_bytes(unit)
                    }
                });
                char::decode_utf16(units)
                    .collect::<std::result::Result<String, _>>()
                    .ok()
                    .map(|s| Sha256::digest(s.as_bytes()))
            }
            _ => None,
        };
        Ok(digest.map(|h| format!("{h:x}")))
    })?;
    db.create_scalar_function("legacy_role", 1, flags, |ctx| {
        let dir: Option<String> = ctx.get(0)?;
        let name = Path::new(dir.as_deref().unwrap_or(""))
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let clean = name
            .split(|c: char| c.is_whitespace() || c.is_ascii_control())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let short: String = clean.chars().take(120).collect();
        Ok(short
            .rsplit('-')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("worker")
            .to_owned())
    })?;
    Ok(())
}

fn repair(db: &Connection, ids: Vec<String>) -> Result<()> {
    db.execute(
        &format!(
            "UPDATE message SET
        message_uid=coalesce(nullif(message_uid,''),'m-'||new_uuid()),
        body_hash=coalesce(nullif(body_hash,''),{BODY_SHA}),
        status_version=coalesce(nullif(status_version,0),1)+CASE WHEN {DELIVERY_CONFLICT}
            THEN 1 ELSE 0 END,
        state=CASE WHEN coalesce(state,'') IN ('','received','unsent') THEN
            CASE WHEN delivered THEN 'received' ELSE 'unsent' END ELSE state END
        WHERE {INCOMPLETE_RECEIPT} OR {DELIVERY_CONFLICT}"
        ),
        [],
    )?;

    crate::recovery::recover_missing(db, ids)?;
    db.execute(&format!("UPDATE dispatch SET
        backend=coalesce(nullif(backend,''),'tmux'), scope=coalesce(nullif(scope,''),'s-'||substr(new_uuid(),1,8)),
        role=coalesce(nullif(role,''),legacy_role(report_dir)), current_round=coalesce(nullif(current_round,0),1),
        current_attempt=coalesce(nullif(current_attempt,''),'a-'||substr(new_uuid(),1,12))
        WHERE {IDENTITY}"), [])?;
    db.execute(&format!("INSERT INTO dispatch_round (dispatch,round,attempt,task,report,state,created)
        SELECT id,current_round,current_attempt,task,report,'running',coalesce(created,unixepoch('subsec'))
        FROM dispatch d WHERE {NO_SNAPSHOT}"), [])?;
    Ok(())
}

/// Open a quiescent working copy for writing. All schema changes and repairs commit
/// together. A populated database gets a durable pre-change backup before the first
/// change. Return its path (if made) and the upgraded connection to the caller.
pub fn open_offline_copy(state: &Path) -> Result<(Connection, Option<PathBuf>)> {
    let state = resolve(state)?;
    fs::create_dir_all(&state)?;
    let path = state.join("router.db");
    #[cfg(unix)]
    if let Ok(meta) = fs::symlink_metadata(&path) {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.is_file() && meta.nlink() == 1,
            "router.db must be an independent regular file in the offline copy"
        );
    }
    let mut db = open(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (gaps, has_ledger) = schema::inspect(&tx)?;
    let mut original = None;
    let mut preserve = || -> Result<()> {
        if has_ledger && original.is_none() {
            original = Some(backup(&state).context("preserve pre-migration router.db")?);
        }
        Ok(())
    };
    register_functions(&tx)?;
    let upgrade = !gaps.is_empty();
    if upgrade {
        preserve()?;
        tx.execute_batch(schema::SCHEMA)?;
        for gap in gaps {
            gap.upgrade(&tx)?;
        }
    }

    // Python hashes the exact UTF-8 text stored in message.body. Validate its
    // surviving witness before any query uses a receipt to reconstruct state.
    let corrupt = tx
        .query_row(
            &format!(
                "SELECT dispatch,id,{BODY_SHA} IS NULL FROM message WHERE CASE
                WHEN nullif(body_hash,'') IS NULL THEN {BODY_SHA} IS NULL
                ELSE {BODY_SHA} IS NOT body_hash END LIMIT 1"
            ),
            [],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((id, message, unreadable)) = corrupt {
        preserve()?;
        let reason = if unreadable {
            "unreadable message body: expected UTF-8 text"
        } else {
            "body hash mismatch"
        };
        bail!(
            "dispatch {}, message {message}: {reason}",
            id.as_deref().unwrap_or("<missing>")
        );
    }

    let ids = crate::recovery::candidates(&tx)?;
    if upgrade || !ids.is_empty() || has_repairs(&tx)? {
        preserve()?;
        repair(&tx, ids).with_context(|| format!("repair offline copy in {}", state.display()))?;
    }
    // A current identity authorizes writes only when that exact snapshot exists.
    let mismatch = tx
        .query_row(
            "SELECT d.id, d.current_round, d.current_attempt FROM dispatch d
        WHERE NOT EXISTS (SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id
            AND r.round=d.current_round AND r.attempt=d.current_attempt) LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((id, round, attempt)) = mismatch {
        preserve()?;
        bail!("dispatch {id}, round {round}, attempt {attempt} has no matching current snapshot");
    }
    let completions = crate::completion::plan(&tx);
    if !completions.as_ref().is_ok_and(|p| p.is_empty()) {
        preserve()?;
    }
    completions?.apply(&tx)?;
    tx.commit()?;
    Ok((db, original))
}
