//! Convert admitted receipts and keep worker-facing messages and faults as history.
use super::{matching_round, messages, text, valid_identity};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

fn checked_body(record: &Value, row: &Value) -> Result<Value> {
    let raw = text(row, "body")?;
    ensure!(
        messages::body_hash(raw) == text(row, "bodyHash")?,
        "legacy message body hash mismatch"
    );
    let body: Value = serde_json::from_str(raw).context("read legacy message body")?;
    ensure!(
        body.is_object()
            && ["dispatch", "scope", "role"]
                .into_iter()
                .all(|key| body.get(key).is_none_or(|value| value == &record[key])),
        "legacy message identity mismatch"
    );
    let state = text(row, "state")?;
    let delivered = row["delivered"]
        .as_i64()
        .context("invalid legacy delivery flag")?;
    ensure!(
        (matches!(state, "received" | "processed") && delivered == 1)
            || (matches!(state, "unknown" | "unsent") && delivered == 0),
        "invalid legacy delivery state"
    );
    ensure!(
        valid_identity(text(row, "messageId")?, "m-")
            && row["id"].as_u64().is_some_and(|id| id > 0)
            && row["statusVersion"]
                .as_u64()
                .is_some_and(|version| version > 0),
        "invalid legacy receipt identity"
    );
    Ok(body)
}

fn current_resolution(row: &Value) -> Option<Value> {
    (!row["resolver_actor"].is_null()).then(|| json!({"actor":row["resolver_actor"],"source":row["resolution_source"],
        "reason":row["resolution_reason"],"evidence":row["resolution_evidence"],"at":row["resolvedAt"]}))
}

fn controller_receipt(record: &Value, row: &Value, body: &Value) -> Result<Value> {
    let done = row["type"] == "done";
    let report = done.then(|| text(body, "report")).transpose()?;
    let round = matching_round(record, body, report)?;
    ensure!(
        !done || round["state"] == "completed",
        "legacy done is attached to a non-completed round"
    );
    let mut message = json!({"dispatch":record["dispatch"],"role":record["role"],"scope":record["scope"],
        "round":round["round"],"attempt":round["attempt"],"id":row["id"],"messageId":row["messageId"],
        "state":row["state"],"statusVersion":row["statusVersion"],"body_hash":row["bodyHash"],
        "legacy_body":row["body"],"request_key":row["request_key"],"created_at":row["at"],"legacyCreated":row["created"]});
    if let Some(report) = report {
        message["report"] = json!(report);
        for field in ["head", "dirty"] {
            if let Some(value) = body.get(field) {
                message[field] = value.clone();
            }
        }
    } else {
        let question = body["question"]
            .as_str()
            .context("invalid legacy question")?;
        message["question_payload"] = json!({"text":question,"options":body["options"]});
    }
    if let Some(resolution) = current_resolution(row) {
        message["resolution"] = resolution;
    }
    Ok(message)
}

fn archived_receipt(
    record: &Value,
    row: &Value,
    body: Value,
    questions: &BTreeMap<u64, usize>,
    native: &[Value],
) -> Result<Value> {
    let recipient = text(row, "recipient")?;
    let kind = text(row, "type")?;
    let mut question_id = None;
    let mut question_uuid = None;
    let round = match (recipient, kind) {
        ("worker", "reply") => {
            let id = body["to"]
                .as_u64()
                .context("legacy reply has no numeric question reference")?;
            let question = questions
                .get(&id)
                .map(|index| &native[*index])
                .context("legacy reply has no earlier controller question")?;
            ensure!(
                question["dispatch"] == record["dispatch"]
                    && body["answer"].is_string()
                    && ["round", "attempt"]
                        .into_iter()
                        .all(|key| body.get(key).is_none_or(|v| v == &question[key])),
                "legacy reply conflicts with its question assignment"
            );
            question_id = Some(id);
            question_uuid = question["messageId"].as_str();
            Some(matching_round(record, question, None)?)
        }
        ("worker", "continue" | "resumed" | "interrupted" | "closed" | "handoff") => {
            ensure!(
                body["round"].is_u64() && body["attempt"].is_string(),
                "legacy {kind} has no attempt identity"
            );
            let report = (kind == "continue")
                .then(|| text(&body, "report"))
                .transpose()?;
            let round = matching_round(record, &body, report)?;
            if kind == "continue" {
                ensure!(
                    text(&body, "task")? == text(round, "task")?,
                    "legacy continue task conflicts with its round"
                );
            }
            if kind == "handoff" {
                let from = text(&body, "fromAttempt")?;
                ensure!(
                    record["rounds"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["attempt"] == from),
                    "legacy handoff has no original attempt"
                );
            }
            Some(round)
        }
        ("worker", "nudge" | "reclaimed")
        | ("controller", "unknown" | "exited" | "blocked" | "stalled") => {
            if kind == "nudge" {
                ensure!(body["text"].is_string(), "invalid legacy nudge");
            }
            if kind == "reclaimed" {
                text(&body, "tmux")?;
            }
            // These publishers did not write execution identity. When it is
            // absent, keep the verified dispatch association without guessing.
            if body.get("round").is_some() || body.get("attempt").is_some() {
                Some(matching_round(record, &body, None)?)
            } else {
                None
            }
        }
        _ => anyhow::bail!("unsupported legacy message type or recipient: {recipient}/{kind}"),
    };
    let mut view = body;
    let fields = view.as_object_mut().unwrap();
    for key in ["questionMessageId", "delivered", "resolution"] {
        fields.remove(key);
    }
    for key in [
        "id",
        "messageId",
        "type",
        "recipient",
        "body",
        "bodyHash",
        "at",
        "state",
        "statusVersion",
    ] {
        view[key] = row[key].clone();
    }
    view["dispatch"] = record["dispatch"].clone();
    view["scope"] = record["scope"].clone();
    view["role"] = record["role"].clone();
    view["round"] = round.map_or(Value::Null, |r| r["round"].clone());
    view["attempt"] = round.map_or(Value::Null, |r| r["attempt"].clone());
    view["to"] = question_id.map_or_else(|| json!(recipient), |id| json!(id));
    if let Some(uuid) = question_uuid {
        view["questionMessageId"] = json!(uuid);
    }
    if recipient == "controller" {
        view["delivered"] = json!(row["delivered"] == 1);
    }
    if let Some(resolution) = current_resolution(row) {
        view["resolution"] = resolution;
    }
    view["requestKey"] = row["request_key"].clone();
    view["legacyCreated"] = row["created"].clone();
    view["legacyArchiveOnly"] = json!(true);
    Ok(view)
}

pub(super) fn receipts(
    records: &mut BTreeMap<String, Value>,
    rows: &[Value],
    resolutions: Vec<Value>,
) -> Result<Vec<Value>> {
    let mut native = Vec::new();
    let mut questions = BTreeMap::new();
    let mut by_id = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let dispatch = text(row, "dispatch")?;
        let message_id = text(row, "messageId")?;
        ensure!(seen.insert(message_id), "duplicate legacy messageId");
        let record = records
            .get_mut(dispatch)
            .with_context(|| format!("legacy receipt has no dispatch {dispatch}"))?;
        let context = || format!("legacy dispatch {dispatch} receipt {}", row["id"]);
        let body = checked_body(record, row).with_context(context)?;
        let id = row["id"].as_u64().unwrap();
        ensure!(
            by_id.insert(id, row).is_none(),
            "duplicate legacy numeric receipt ID"
        );
        if row["recipient"] == "controller"
            && matches!(row["type"].as_str(), Some("done" | "question"))
        {
            let message = controller_receipt(record, row, &body).with_context(context)?;
            if row["type"] == "question" {
                questions.insert(id, native.len());
            }
            native.push(message);
        } else {
            let message =
                archived_receipt(record, row, body, &questions, &native).with_context(context)?;
            record["messages"].as_array_mut().unwrap().push(message);
        }
    }
    for mut resolution in resolutions {
        let row = resolution["message"]
            .as_u64()
            .and_then(|id| by_id.get(&id))
            .context("legacy resolution has no referenced receipt")?;
        ensure!(
            resolution["id"].as_u64().is_some_and(|id| id > 0)
                && resolution["created"].is_number()
                && resolution["bodyHash"] == row["bodyHash"]
                && row["recipient"] == "controller"
                && resolution["fromState"] == "unknown"
                && resolution["toState"] == "received"
                && matches!(row["state"].as_str(), Some("received" | "processed")),
            "legacy resolution conflicts with its receipt"
        );
        if let Some(current) = current_resolution(row) {
            ensure!(
                ["actor", "source", "reason", "evidence"]
                    .into_iter()
                    .all(|key| current[key] == resolution[key]),
                "legacy resolution conflicts with its recorded receipt evidence"
            );
        }
        resolution["messageId"] = row["messageId"].clone();
        let record = records.get_mut(text(row, "dispatch")?).unwrap();
        record["legacyResolutions"]
            .as_array_mut()
            .unwrap()
            .push(resolution);
    }
    Ok(native)
}
