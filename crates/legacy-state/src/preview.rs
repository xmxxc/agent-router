use crate::{open, resolve, schema};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{types::ValueRef, Connection, OpenFlags, Params};
use serde_json::{json, Map, Value};
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
};

const SCHEMA_NOTE: &str = "Older router.db schema. Missing fields are shown as null; migrate an offline copy to fill them.";
const BACKEND_NOTE: &str = "Offline ledger. Verify the original backend identity and worker callback before taking control.";
const MESSAGE: &str = "SELECT id,coalesce(nullif(message_uid,''),cast(id AS text)) AS messageId,
    dispatch,type,recipient AS 'to',datetime(created,'unixepoch','localtime') AS 'at',
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
        if let Some(body) = map["body"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Map<String, Value>>(s).ok())
        {
            // The envelope owns identity fields. Keep the exact stored text as well.
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
    db.execute_batch("PRAGMA temp_store=MEMORY; BEGIN")?;
    let (gaps, _) = schema::inspect(&db)?;
    for gap in &gaps {
        gap.project(&db)?;
    }
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
    let mut dispatches = rows(&db, "SELECT id AS dispatch,scope,role,label,host,model,cwd,task,report,
        host_session AS hostSession,started,current_round AS round,current_attempt AS attempt,
        coalesce(nullif(backend,''),'tmux') AS backend,backend_handle AS backendHandle,
        launch_receipt AS launchReceipt FROM dispatch WHERE ?1 IS NULL OR id=?1 ORDER BY created,id", [dispatch])?;
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
        let current = rounds.iter().find(|r| {
            !r["attempt"].is_null()
                && r["attempt"] == item["attempt"]
                && r["round"] == item["round"]
        });
        item["status"] = current
            .map(|r| r["state"].clone())
            .unwrap_or(json!("unknown"));
        item["rounds"] = json!(rounds);
        item["audit"] = json!(rows(&db, "SELECT id,operation,round,attempt,from_attempt AS fromAttempt,
            to_attempt AS toAttempt,from_host AS fromHost,to_host AS toHost,result,diagnostic,
            owner_generation AS ownerGeneration,resolver_actor AS resolverActor,resolution_evidence AS resolutionEvidence,
            resolved_at AS resolvedAt,consumed_at AS consumedAt,created FROM lifecycle_event WHERE dispatch=? ORDER BY id", [&id])?);
        if let Some(fault) = rows(&db, "SELECT operation,outcome,category,diagnostic,created FROM fault_event WHERE dispatch=?", [&id])?.pop() {
            item["fault"] = fault;
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
        item["diagnostic"] = json!(BACKEND_NOTE);
        if dispatch.is_some() {
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
        "SELECT d.id AS dispatch,d.label,coalesce(nullif(d.backend,''),'tmux') AS backend,
        coalesce(r.state,'unknown') AS state FROM dispatch d LEFT JOIN dispatch_round r
        ON r.dispatch=d.id AND r.round=d.current_round AND r.attempt=d.current_attempt
        WHERE (?1 IS NULL AND r.state='completed') OR d.id=?1 ORDER BY d.created,d.id",
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
        item["diagnostic"] = json!(BACKEND_NOTE);
    }
    preview["sessions"] = json!(sessions);
    Ok(preview)
}
