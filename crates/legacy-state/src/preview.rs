use crate::{open, resolve, schema};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{functions::FunctionFlags, types::ValueRef, Connection, OpenFlags, Params};
use serde_json::{json, Map, Value};
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
};

const SCHEMA_NOTE: &str = "Older router.db schema. Missing fields are shown as null; migrate an offline copy to fill them.";
const MESSAGE: &str = "SELECT id,coalesce(nullif(message_uid,''),cast(id AS text)) AS messageId,
    dispatch,type,recipient AS 'to',datetime(epoch_second(created),'unixepoch','localtime') AS 'at',
    coalesce(nullif(state,''),CASE WHEN delivered THEN 'received' ELSE 'unsent' END) AS state,
    coalesce(nullif(status_version,0),1) AS statusVersion,body_hash AS bodyHash,body,delivered,
    resolver_actor,resolution_source,resolution_reason,resolution_evidence,resolved_at FROM message";

fn rows(db: &Connection, sql: &str, params: impl Params) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let mut cursor = stmt.query(params)?;
    let mut items = vec![];
    while let Some(row) = cursor.next()? {
        let mut item = Map::new();
        for (i, name) in row.as_ref().column_names().iter().enumerate() {
            let value = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => json!(std::str::from_utf8(s)?),
                ValueRef::Blob(_) => bail!("unexpected blob in ledger column {name}"),
            };
            item.insert(name.to_string(), value);
        }
        items.push(Value::Object(item));
    }
    Ok(items)
}

fn messages(db: &Connection, filter: &str, id: Option<&str>) -> Result<Vec<Value>> {
    let mut items = rows(db, &format!("{MESSAGE} WHERE {filter} ORDER BY id"), [id])?;
    for item in &mut items {
        let map = item.as_object_mut().unwrap();
        if let Some(mut body) = map["body"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Map<String, Value>>(s).ok())
        {
            // A Python reply uses "to" for the original question's numeric id.
            if map["type"] == "reply" && body.get("to").is_some_and(Value::is_i64) {
                map.insert("to".into(), body.remove("to").unwrap());
            }
            // Other payload fields cannot replace receipt identity or the stored text.
            for (key, val) in body {
                map.entry(key).or_insert(val);
            }
        }
        let delivered = map.remove("delivered").unwrap();
        if map["to"] == "controller" {
            map.insert(
                "delivered".into(),
                json!(delivered.as_i64().unwrap_or(0) != 0),
            );
        }
        let actor = map.remove("resolver_actor").unwrap();
        let resolution = json!({"actor":actor,"source":map.remove("resolution_source"),
            "reason":map.remove("resolution_reason"),"evidence":map.remove("resolution_evidence"),
            "at":map.remove("resolved_at")});
        if !actor.is_null() {
            map.insert("resolution".into(), resolution);
        }
    }
    Ok(items)
}

fn audit(db: &Connection, id: &str) -> Result<Vec<Value>> {
    let mut events = rows(db, "SELECT id,operation,round,attempt,from_attempt AS fromAttempt,
        to_attempt AS toAttempt,from_host AS fromHost,to_host AS toHost,result,diagnostic,
        owner_generation AS ownerGeneration,resolver_actor AS resolverActor,resolution_evidence AS resolutionEvidence,
        resolution_owner_generation AS resolutionOwnerGeneration,resolved_at AS resolvedAt,consumed_at AS consumedAt,
        datetime(epoch_second(resolved_at),'unixepoch','localtime') AS resolvedTime,
        datetime(epoch_second(consumed_at),'unixepoch','localtime') AS consumedTime,created
        FROM lifecycle_event WHERE dispatch=? ORDER BY id", [id])?;
    for event in &mut events {
        let fields = event.as_object_mut().unwrap();
        let at = fields.remove("resolvedTime");
        let consumed = fields.remove("consumedTime").unwrap();
        if fields["resolverActor"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            let mut resolution = json!({"actor":fields["resolverActor"],"evidence":fields["resolutionEvidence"],
                "ownerGeneration":fields["resolutionOwnerGeneration"],"at":at});
            if !consumed.is_null() {
                resolution["consumedAt"] = consumed;
            }
            fields.insert("resolution".into(), resolution);
        }
    }
    Ok(events)
}

fn reader(state: &Path) -> Result<Option<(Connection, bool)>> {
    let path = state.join("router.db");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // A read-only SQLite connection can still create WAL sidecars in a writable
    // directory. Python uses rollback journaling. Diagnose WAL before opening it.
    let mut header = Vec::new();
    file.take(20).read_to_end(&mut header)?;
    ensure!(header.get(18..20) != Some(&[2,2]), "WAL-mode ledger: use a consistent SQLite backup in a private directory and set that copy's journal_mode=DELETE before offline preview");
    let db = open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Python local_time drops fractions before formatting and displays zero as null.
    db.create_scalar_function(
        "epoch_second",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_DIRECTONLY,
        |ctx| {
            Ok(ctx
                .get::<Option<f64>>(0)?
                .filter(|t| *t != 0.0)
                .map(f64::floor))
        },
    )?;
    db.execute_batch("PRAGMA temp_store=MEMORY; BEGIN")?;
    let (gaps, _) = schema::inspect(&db)?;
    for gap in &gaps {
        gap.project(&db)?;
    }
    // A historical dispatch can survive only in evidence tables. Join the parent
    // optionally, so a missing row never supplies launch defaults or current identity.
    db.execute_batch("CREATE TEMP VIEW offline_dispatch AS
        WITH ids(id) AS (SELECT id FROM dispatch UNION SELECT dispatch FROM message
            UNION SELECT dispatch FROM dispatch_round UNION SELECT dispatch FROM fault_event
            UNION SELECT dispatch FROM lifecycle_event UNION SELECT dispatch FROM watchdog_fault
            UNION SELECT dispatch FROM host_error)
        SELECT ids.id AS history_id, d.*,
            CASE WHEN d.id IS NOT NULL THEN coalesce(nullif(d.backend,''),'tmux') END AS known_backend,
            coalesce(r.state,'unknown') AS current_state,
            CASE WHEN d.id IS NULL THEN 'Missing dispatch row. Current attempt and backend identity are unknown.'
                 WHEN r.dispatch IS NULL AND EXISTS (SELECT 1 FROM dispatch_round h WHERE h.dispatch=d.id)
                    THEN 'Current identity has no matching attempt snapshot. Inspect the original ledger.'
                 ELSE 'Offline ledger. Verify the original backend identity and worker callback before taking control.'
            END AS diagnostic
        FROM ids LEFT JOIN dispatch d ON d.id=ids.id
            LEFT JOIN dispatch_round r ON r.dispatch=d.id AND r.round=d.current_round AND r.attempt=d.current_attempt
        WHERE ids.id IS NOT NULL")?;
    Ok(Some((db, !gaps.is_empty())))
}

/// Read durable dispatches, receipts and historical waiter identities. Liveness is
/// unknown in a copied ledger. Backend identities are never passed to a process.
pub fn status(state: &Path, dispatch: Option<&str>, active: bool) -> Result<Value> {
    ensure!(
        !active || dispatch.is_none(),
        "--active and --dispatch are exclusive"
    );
    let state = resolve(state)?;
    let mut result = json!({"state":state,"dispatches":[],"undelivered":[],"waiters":[]});
    if active {
        result["reclaimable"] = json!([]);
    }
    let Some((db, older)) = reader(&state)? else {
        ensure!(
            dispatch.is_none(),
            "unknown dispatch {}",
            dispatch.unwrap_or_default()
        );
        return Ok(result);
    };
    if older {
        result["schema"] = json!(SCHEMA_NOTE);
    }
    let mut dispatches = rows(&db, "SELECT history_id AS dispatch, id IS NOT NULL AS dispatchRecorded, scope,role,label,host,model,cwd,task,report,
        host_session AS hostSession,started,current_round AS round,current_attempt AS attempt,
        known_backend AS backend,backend_handle AS backendHandle,current_state AS status,diagnostic,
        launch_receipt AS launchReceipt FROM offline_dispatch WHERE ?1 IS NULL OR history_id=?1 ORDER BY created,history_id", [dispatch])?;
    ensure!(
        dispatch.is_none() || !dispatches.is_empty(),
        "unknown dispatch {}",
        dispatch.unwrap_or_default()
    );
    for item in &mut dispatches {
        let id = item["dispatch"]
            .as_str()
            .context("missing dispatch id")?
            .to_owned();
        let rounds = rows(
            &db,
            "SELECT round,attempt,task,report,state,result,created,completed
            FROM dispatch_round WHERE dispatch=? ORDER BY round,created,attempt",
            [&id],
        )?;
        item["rounds"] = json!(rounds);
        item["audit"] = json!(audit(&db, &id)?);
        if let Some(fault) = rows(&db, "SELECT operation,outcome,category,diagnostic,created FROM fault_event WHERE dispatch=?", [&id])?.pop() {
            item["fault"] = fault;
        }
        let watchdog = rows(
            &db,
            "SELECT round,attempt,kind,fingerprint FROM watchdog_fault WHERE dispatch=?
            ORDER BY round,attempt,kind,fingerprint",
            [&id],
        )?;
        if !watchdog.is_empty() {
            item["watchdogFaults"] = json!(watchdog);
        }
        if let Some(error) = rows(&db, "SELECT turn FROM host_error WHERE dispatch=?", [&id])?.pop()
        {
            item["hostError"] = error;
        }
        if let Some(owner) = rows(
            &db,
            "SELECT owner,generation,lease_until AS leaseUntil FROM controller_owner WHERE scope=?",
            [item["scope"].as_str()],
        )?
        .pop()
        {
            item["controllerOwner"] = owner;
        }
        if let Some(handle) = item["backendHandle"].as_str() {
            if let Ok(parsed) = serde_json::from_str(handle) {
                item["backendHandle"] = parsed;
            }
        }
        item["backendAlive"] = Value::Null;
        let recorded = item["dispatchRecorded"] == 1;
        item["dispatchRecorded"] = json!(recorded);
        if dispatch.is_some() || !recorded {
            item["messages"] = json!(messages(&db, "dispatch=?1", Some(&id))?);
        }
    }
    if active {
        dispatches.retain(|d| d["status"] != "completed");
    }
    result["dispatches"] = json!(dispatches);
    result["undelivered"] = json!(messages(
        &db,
        "recipient='controller' AND delivered=0 AND (?1 IS NULL OR dispatch=?1)",
        dispatch
    )?);
    result["waiters"] = json!(rows(&db, "SELECT pid,kind,since,process_start AS processStart,'unknown' AS liveness,
        'Recorded process identity only; verify the original relay or waiter before cutover.' AS diagnostic FROM waiter ORDER BY since", [])?);
    Ok(result)
}

/// A copied database alone cannot prove a session belongs to this router.
pub fn reclaim_preview(state: &Path, dispatch: Option<&str>) -> Result<Value> {
    let state = resolve(state)?;
    let mut preview = json!({"state":state,"yes":false,"sessions":[]});
    let Some((db, older)) = reader(&state)? else {
        ensure!(
            dispatch.is_none(),
            "unknown dispatch {}",
            dispatch.unwrap_or_default()
        );
        return Ok(preview);
    };
    if older {
        preview["schema"] = json!(SCHEMA_NOTE);
    }
    let mut sessions = rows(
        &db,
        "SELECT history_id AS dispatch,label,known_backend AS backend,current_state AS state,diagnostic
         FROM offline_dispatch WHERE (?1 IS NULL AND current_state='completed') OR history_id=?1
         ORDER BY created,history_id",
        [dispatch],
    )?;
    ensure!(
        dispatch.is_none() || !sessions.is_empty(),
        "unknown dispatch {}",
        dispatch.unwrap_or_default()
    );
    for item in &mut sessions {
        item["reclaimable"] = json!(false);
        item["result"] = json!("unknown");
    }
    preview["sessions"] = json!(sessions);
    Ok(preview)
}
