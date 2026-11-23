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

const INCOMPLETE_RECEIPT: &str = "coalesce(message_uid,'')='' OR coalesce(body_hash,'')='' OR
    coalesce(state,'')='' OR coalesce(status_version,0)=0";
const DELIVERY_CONFLICT: &str =
    "(coalesce(delivered,0)=0 AND state='received') OR (delivered<>0 AND state='unsent')";
const IDENTITY: &str = "coalesce(backend,'')='' OR coalesce(scope,'')='' OR coalesce(role,'')=''
    OR coalesce(current_round,0)=0 OR coalesce(current_attempt,'')=''";
const NO_SNAPSHOT: &str = "NOT EXISTS (SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id)";

// Pre-attempt receipts can identify a snapshot by report only when that match is unique.
const DONE_MATCH: &str = "FROM dispatch_round r JOIN message m ON m.dispatch=r.dispatch
    WHERE r.state='running' AND m.type='done' AND json_valid(m.body)
    AND json_extract(m.body,'$.report')=r.report
    AND (json_extract(m.body,'$.round') IS NULL OR json_extract(m.body,'$.round')=r.round)
    AND (json_extract(m.body,'$.attempt')=r.attempt OR
        (json_extract(m.body,'$.attempt') IS NULL AND
         (SELECT count(*) FROM dispatch_round other WHERE other.dispatch=r.dispatch
            AND other.report=r.report)=1))";

fn has_repairs(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM message WHERE {INCOMPLETE_RECEIPT} OR {DELIVERY_CONFLICT})
        OR EXISTS(SELECT 1 FROM dispatch d WHERE {IDENTITY} OR {NO_SNAPSHOT})
        OR EXISTS(SELECT 1 {DONE_MATCH})"
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
    db.create_scalar_function("body_sha", 1, flags, |ctx| {
        let body: String = ctx.get(0)?;
        Ok(format!("{:x}", Sha256::digest(body.as_bytes())))
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

fn repair(db: &Connection) -> Result<()> {
    register_functions(db)?;
    db.execute(
        &format!(
            "UPDATE message SET
        message_uid=coalesce(nullif(message_uid,''),'m-'||new_uuid()),
        body_hash=coalesce(nullif(body_hash,''),body_sha(body)),
        status_version=coalesce(nullif(status_version,0),1)+CASE WHEN {DELIVERY_CONFLICT}
            THEN 1 ELSE 0 END,
        state=CASE WHEN coalesce(state,'') IN ('','received','unsent') THEN
            CASE WHEN delivered THEN 'received' ELSE 'unsent' END ELSE state END
        WHERE {INCOMPLETE_RECEIPT} OR {DELIVERY_CONFLICT}"
        ),
        [],
    )?;

    // Python's oldest dispatches have no history at all. Never invent a new current
    // writer for a partially populated ledger whose prior attempts already exist.
    let ambiguous: Option<String> = db.query_row(
        &format!(
            "SELECT min(id) FROM dispatch d WHERE
        (coalesce(current_attempt,'')='' OR coalesce(current_round,0)=0) AND NOT ({NO_SNAPSHOT})"
        ),
        [],
        |r| r.get(0),
    )?;
    if let Some(id) = ambiguous {
        bail!("dispatch {id} has attempt history but incomplete current identity; inspect the original ledger");
    }
    db.execute(&format!("UPDATE dispatch SET
        backend=coalesce(nullif(backend,''),'tmux'), scope=coalesce(nullif(scope,''),'s-'||substr(new_uuid(),1,8)),
        role=coalesce(nullif(role,''),legacy_role(report_dir)), current_round=coalesce(nullif(current_round,0),1),
        current_attempt=coalesce(nullif(current_attempt,''),'a-'||substr(new_uuid(),1,12))
        WHERE {IDENTITY}"), [])?;
    db.execute(&format!("INSERT INTO dispatch_round (dispatch,round,attempt,task,report,state,created)
        SELECT id,current_round,current_attempt,task,report,'running',coalesce(created,unixepoch('subsec'))
        FROM dispatch d WHERE {NO_SNAPSHOT}"), [])?;
    db.execute(&format!("WITH matches AS MATERIALIZED (
        SELECT r.rowid AS snapshot,min(m.id) AS receipt {DONE_MATCH} GROUP BY r.rowid)
        UPDATE dispatch_round SET state='completed',result=m.body,completed=m.created
        FROM matches JOIN message m ON m.id=matches.receipt WHERE dispatch_round.rowid=matches.snapshot"), [])?;
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
    if !gaps.is_empty() || has_repairs(&tx)? {
        if has_ledger {
            original = Some(backup(&state).context("preserve pre-migration router.db")?);
        }
        tx.execute_batch(schema::SCHEMA)?;
        for gap in gaps {
            gap.upgrade(&tx)?;
        }
        repair(&tx).with_context(|| format!("repair offline copy in {}", state.display()))?;
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
        bail!("dispatch {id}, round {round}, attempt {attempt} has no matching current snapshot");
    }
    tx.commit()?;
    Ok((db, original))
}
