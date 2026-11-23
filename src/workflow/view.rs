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
                for field in ["round", "attempt", "report", "type"] {
                    ensure!(
                        original[field].is_null()
                            || message[field].is_null()
                            || original[field] == message[field],
                        "conflicting {field} for message {key}"
                    );
                }
                let newer_delivery = message["statusVersion"].as_u64().unwrap_or(0)
                    >= original["statusVersion"].as_u64().unwrap_or(0);
                for (k, v) in message.as_object().unwrap() {
                    if original.get(k).is_none()
                        || (original[k].is_null()
                            && matches!(k.as_str(), "round" | "attempt" | "type" | "report"))
                        || k == "id"
                        || (newer_delivery
                            && matches!(
                                k.as_str(),
                                "state" | "statusVersion" | "delivered" | "resolution"
                            ))
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
            let state = match row["worker"]["state"].as_str() {
                Some("running") => "running",
                Some("cancelled") => "cancelled",
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
    let (state, status) = match current.as_str() {
        "completed" => ("done", "completed"),
        "running" if alive == Some(true) => ("working", "online"),
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
