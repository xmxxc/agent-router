//! Project the task outcome independently of the live process and message delivery.
use anyhow::{ensure, Result};
use serde_json::{json, Value};

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
    let prior_exit = super::backend::pending_herdr_close(row)
        .filter(|event| {
            pending_close && super::backend::verify_herdr_close_target(row, event).is_ok()
        })
        .map(|event| &event["workerOnClose"])
        .filter(|worker| worker["state"] == "exited")
        .cloned();
    let unresolved_worker = pending_close && prior_exit.is_none();
    if let Some(exit) = prior_exit {
        row["worker"] = exit;
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
            let state = match row["worker"]["state"].as_str() {
                _ if unresolved_worker => "unknown",
                _ if unconfirmed_create && row["worker"]["state"] != "cancelled" => {
                    round["result"] = row["create"].clone();
                    "unknown"
                }
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
