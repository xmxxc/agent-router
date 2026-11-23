//! Project the task outcome independently of the live process and message delivery.
use anyhow::{ensure, Result};
use serde_json::{json, Value};

// Archive order fixes the first completion for each round. Delivery state
// can advance in the live ledger; its numeric IDs belong to that ledger.
pub fn merged_history(row: &Value, ledger: Vec<Value>) -> Result<Vec<Value>> {
    let mut history = Vec::<Value>::new();
    let mut positions = std::collections::HashMap::<String, usize>::new();
    for message in row["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .chain(ledger)
    {
        ensure!(
            message.is_object() && message["dispatch"] == row["dispatch"],
            "message assignment mismatch"
        );
        if let Some(key) = message["messageId"].as_str() {
            if let Some(&pos) = positions.get(key) {
                let original = &mut history[pos];
                let identity_fields = ["round", "attempt", "report", "type", "question", "options"];
                for field in identity_fields {
                    ensure!(
                        original[field].is_null()
                            || message[field].is_null()
                            || original[field] == message[field],
                        "conflicting {field} for message {key}"
                    );
                }
                let delivery_fields = ["state", "statusVersion", "delivered", "resolution"];
                if message["statusVersion"].as_u64().unwrap_or(0)
                    >= original["statusVersion"].as_u64().unwrap_or(0)
                {
                    for key in delivery_fields {
                        if let Some(value) = message.get(key) {
                            original[key] = value.clone();
                        } else {
                            original.as_object_mut().unwrap().remove(key);
                        }
                    }
                }
                // Older archived views can omit this derived flag. Derive it
                // from the chosen version, never from another delivery snapshot.
                match original["state"].as_str() {
                    Some("received" | "processed") => original["delivered"] = json!(true),
                    Some("unsent" | "unknown") => original["delivered"] = json!(false),
                    _ => {}
                }
                for (k, v) in message.as_object().unwrap() {
                    if !delivery_fields.contains(&k.as_str())
                        && (original.get(k).is_none()
                            || (original[k].is_null() && identity_fields.contains(&k.as_str()))
                            || k == "id")
                    {
                        original[k] = v.clone();
                    }
                }
                continue;
            }
            positions.insert(key.to_owned(), history.len());
        }
        history.push(message);
    }
    history.sort_by_key(|m| m["round"].as_u64().unwrap_or(0));
    Ok(history)
}

pub fn project(row: &mut Value, history: &[Value], alive: Option<bool>, named: bool) -> Result<()> {
    let mut rounds = row
        .get("rounds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !rounds
        .iter()
        .any(|r| r["attempt"] == row["attempt"] && r["round"] == row["round"])
    {
        rounds.push(json!({"round":row["round"],"attempt":row["attempt"],"task":row["task"],"report":row["report"],
            "created":row["created"],"completed":null,"state":"running","result":null}));
    }
    let pending_close = row["closePending"] == true;
    #[cfg(target_os = "linux")]
    let prior_exit = super::backend::pending_herdr_close(row)
        .filter(|event| {
            pending_close && super::backend::verify_herdr_close_target(row, event).is_ok()
        })
        .map(|event| &event["workerOnClose"])
        .filter(|worker| worker["state"] == "exited" && worker["exit_code"].is_i64())
        .cloned();
    #[cfg(not(target_os = "linux"))]
    let prior_exit: Option<Value> = None;
    let unresolved_worker =
        (pending_close || row["closeWorkerDiagnostic"].is_string()) && prior_exit.is_none();
    if let Some(exit) = prior_exit {
        row["worker"] = exit;
    }
    let startup_failure = history.iter().find(|message| {
        message["kind"] == "startup_failure"
            && message["dispatch"] == row["dispatch"]
            && message["round"] == row["round"]
            && message["attempt"] == row["attempt"]
    });
    if let Some(failure) = startup_failure {
        row["startupFailure"] = failure["fault"].clone();
        if row["worker"]["state"] == "unknown"
            && matches!(
                failure["fault"]["category"].as_str(),
                Some("executable_missing" | "permission_denied" | "spawn_failed")
            )
        {
            row["worker"] = json!({"state":"not_started","exit_code":null,"signal":null});
        }
    }
    let mut current = "unknown".to_owned();
    for round in &mut rounds {
        ensure!(round.is_object(), "invalid stored round");
        let same = round["round"] == row["round"] && round["attempt"] == row["attempt"];
        let first = history.iter().find(|m| {
            m["type"] == "done"
                && m["dispatch"] == row["dispatch"]
                && m["attempt"] == round["attempt"]
                && m["round"] == round["round"]
                && (round["report"].is_null() || m["report"] == round["report"])
        });
        if let Some(m) = first {
            if !matches!(round["state"].as_str(), Some("failed" | "cancelled"))
                && (round["state"] != "completed" || round["result"].is_null())
            {
                let mut result = json!({});
                for key in [
                    "id",
                    "messageId",
                    "scope",
                    "role",
                    "round",
                    "attempt",
                    "report",
                    "head",
                    "dirty",
                ] {
                    if let Some(value) = m.get(key) {
                        result[key] = value.clone();
                    }
                }
                round["result"] = result;
                round["state"] = json!("completed");
                round["completed"] = m["at"].clone();
            }
        } else if same
            && !matches!(
                round["state"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
        {
            let unconfirmed_create =
                row["backend"] == "herdr" && row["create"]["result"] != "accepted";
            let unconfirmed_resume = super::assignment::resume_audit(row)
                .is_some_and(|event| event["result"] != "accepted");
            let state = match row["worker"]["state"].as_str() {
                _ if unconfirmed_resume => "unknown",
                Some("exited" | "not_started") if startup_failure.is_some() => "failed",
                _ if unresolved_worker => "unknown",
                _ if unconfirmed_create && row["worker"]["state"] != "cancelled" => {
                    round["result"] = row["create"].clone();
                    "unknown"
                }
                Some("running") => "running",
                Some("cancelled" | "not_started") => "cancelled",
                Some("exited") => "failed", // An exit without a done receipt is unfinished work.
                None if row.get("worker").is_none() => "completed", // #4 foreground success
                _ => "unknown",
            };
            round["state"] = json!(state);
            if matches!(state, "failed" | "cancelled") {
                round["result"] = row["worker"].clone();
            }
        }
        if same {
            current = round["state"].as_str().unwrap_or("unknown").to_owned();
        }
    }
    let awaiting_answer = history.iter().any(|m| {
        m["type"] == "question"
            && m["dispatch"] == row["dispatch"]
            && m["round"] == row["round"]
            && m["attempt"] == row["attempt"]
    });
    let (state, status) = match current.as_str() {
        "completed" => ("done", "completed"),
        "running" if alive == Some(true) && awaiting_answer => ("asking", "waiting-human"),
        "running" if alive == Some(true) => ("working", "online"),
        "failed" if row["worker"]["state"] == "not_started" => ("not_started", "failed"),
        "failed" => ("exited", "failed"),
        "cancelled" => ("cancelled", "cancelled"),
        _ => ("unknown", "unknown"),
    };
    row["state"] = json!(state);
    row["status"] = json!(status);
    row["lifecycle"] = json!(if status == "online" {
        "running"
    } else {
        status
    });
    if let Some(event) = history.iter().rev().find(|m| {
        m["type"] == "reclaimed"
            && m["to"] == "worker"
            && m["dispatch"] == row["dispatch"]
            && m["round"] == row["round"]
            && m["attempt"] == row["attempt"]
    }) {
        row["reclaimed"] = event["at"].clone();
        row["status"] = json!("reclaimed");
        row["lifecycle"] = json!("reclaimed");
    }
    row["backendAlive"] = json!(alive);
    if row["backend"] == "tmux" {
        row["tmuxAlive"] = json!(alive);
    }
    row["rounds"] = json!(rounds);
    row["last"] = history.last().cloned().unwrap_or(Value::Null);
    if named {
        row["messages"] = json!(history);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_fields_follow_one_version_in_either_snapshot_order() -> Result<()> {
        let initial = json!({"messageId":"m","dispatch":"d","state":"unsent","statusVersion":1,
            "delivered":false,"resolution":"old","head":"original"});
        let processed = json!({"messageId":"m","dispatch":"d","state":"processed","statusVersion":3,"head":"later"});
        for (first, second) in [(&initial, &processed), (&processed, &initial)] {
            let row = json!({"dispatch":"d","messages":[first]});
            let merged = &merged_history(&row, vec![second.clone()])?[0];
            assert_eq!(merged["statusVersion"], 3);
            assert_eq!(merged["state"], "processed");
            assert_eq!(merged["delivered"], true);
            assert!(merged.get("resolution").is_none());
            assert_eq!(merged["head"], first["head"]);
        }
        Ok(())
    }

    #[test]
    fn archive_and_live_receipts_share_stable_ids_across_delivery_transitions() -> Result<()> {
        let old = json!({"id":1,"messageId":"archived","dispatch":"d-12345678","round":1,"attempt":"old","report":"original"});
        let shared = json!({"id":2,"messageId":"shared","dispatch":"d-12345678","round":2,"attempt":null,"report":null,"state":"unsent","head":null});
        let row = json!({"dispatch":"d-12345678","messages":[old, shared]});
        let claimed = json!({"id":1,"messageId":"shared","dispatch":"d-12345678","round":2,"attempt":"current","report":"second","state":"processed","statusVersion":3,"head":"later"});
        let messages = merged_history(&row, vec![claimed])?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], old);
        assert!(messages[1]["head"].is_null());
        assert_eq!(messages[1]["attempt"], "current");
        assert_eq!(messages[1]["report"], "second");
        assert_eq!(messages[1]["state"], "processed");
        assert_eq!(messages[1]["id"], 1);
        assert_eq!(row["messages"][1], shared);
        Ok(())
    }
}
