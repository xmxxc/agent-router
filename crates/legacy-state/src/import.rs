//! Strict reader for a private #39/#40 offline snapshot.
use super::{open, preview::rows};
use anyhow::{ensure, Result};
use rusqlite::OpenFlags;
use serde_json::Value;
use std::path::Path;

pub struct StoppedLedger {
    pub dispatches: Vec<Value>,
    pub rounds: Vec<Value>,
    pub messages: Vec<Value>,
    pub owners: Vec<Value>,
    pub audit: Vec<Value>,
    pub faults: Vec<Value>,
    pub watchdog_faults: Vec<Value>,
    pub host_errors: Vec<Value>,
    pub screen_watches: Vec<Value>,
    pub resolutions: Vec<Value>,
    pub high_water: u64,
}

pub fn import_scratch(parent: &Path) -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(".wfc-legacy-import-")
        .tempdir_in(parent)?)
}

pub fn stopped_ledger(database: &Path) -> Result<StoppedLedger> {
    let db = open(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN")?;
    let check: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    ensure!(check == "ok", "invalid offline SQLite snapshot");
    ensure!(
        !db.query_row("SELECT EXISTS(SELECT 1 FROM waiter)", [], |row| row
            .get::<_, bool>(0))?,
        "offline snapshot has a recorded waiter or relay"
    );
    ensure!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM dispatch_round WHERE state IS NULL OR state NOT IN ('completed','failed','cancelled'))",
            [], |row| row.get::<_, bool>(0)
        )?,
        "offline snapshot has a nonterminal round"
    );
    ensure!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_event WHERE result IS NULL OR result NOT IN
             ('accepted','completed','failed','cancelled','refused','definitely-not-sent'))",
            [],
            |row| row.get::<_, bool>(0)
        )?,
        "offline snapshot has an unresolved lifecycle event"
    );
    let high_water = db.query_row(
        "SELECT max(coalesce((SELECT max(seq) FROM sqlite_sequence WHERE name='message'),0),
                    coalesce((SELECT max(id) FROM message),0))",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    Ok(StoppedLedger {
        high_water,
        dispatches: rows(&db, "SELECT id AS dispatch,host,backend,cwd,task,created,scope,role,label,
            current_round AS round,current_attempt AS attempt,report FROM dispatch ORDER BY id", [])?,
        rounds: rows(&db, "SELECT dispatch,round,attempt,task,report,state,result,created,completed
            FROM dispatch_round ORDER BY dispatch,round,created,attempt", [])?,
        messages: rows(&db, "SELECT id,dispatch,type,recipient,body,body_hash AS bodyHash,
            message_uid AS messageId,request_key,state,status_version AS statusVersion,delivered,created,
            datetime(CAST(created AS INTEGER),'unixepoch','localtime') AS 'at',
            resolver_actor,resolution_source,resolution_reason,resolution_evidence,resolved_at AS legacyResolvedAt,
            datetime(CAST(resolved_at AS INTEGER),'unixepoch','localtime') AS resolvedAt
            FROM message ORDER BY id", [])?,
        owners: rows(&db, "SELECT scope,owner,token,generation,lease_until FROM controller_owner ORDER BY scope", [])?,
        audit: rows(&db, "SELECT dispatch,id,operation,round,attempt,from_attempt AS fromAttempt,to_attempt AS toAttempt,
            from_host AS fromHost,to_host AS toHost,result,diagnostic,owner_generation AS ownerGeneration,
            owner_token IS NOT NULL AS ownerTokenRecorded,resolver_actor,resolution_evidence,resolution_owner_generation,
            resolution_owner_generation AS resolutionOwnerGeneration,resolved_at,consumed_at,created
            FROM lifecycle_event ORDER BY id", [])?,
        faults: rows(&db, "SELECT dispatch,operation,outcome,category,diagnostic,created
            FROM fault_event ORDER BY dispatch", [])?,
        watchdog_faults: rows(&db, "SELECT dispatch,round,attempt,kind,fingerprint
            FROM watchdog_fault ORDER BY dispatch,round,attempt,kind,fingerprint", [])?,
        host_errors: rows(&db, "SELECT dispatch,turn AS hostError FROM host_error ORDER BY dispatch", [])?,
        screen_watches: rows(&db, "SELECT dispatch,since AS screenObservedSince FROM screen_watch ORDER BY dispatch", [])?,
        resolutions: rows(&db, "SELECT id,message,actor,source,reason,evidence,body_hash AS bodyHash,
            from_state AS fromState,to_state AS toState,created FROM message_resolution ORDER BY id", [])?,
    })
}
