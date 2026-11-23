//! Bind backend operations to the assignment. A durable close intent turns
//! concurrent requests and retries after a lost reply into receipt reads.
use super::{call, closed_path, write_json, Launch};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{env, fs, io::ErrorKind, path::Path};

pub(super) fn create(state: &Path, row: &Value, mut launch: Launch) -> Result<Value> {
    for (name, value) in env::vars_os() {
        let name = name
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid environment name"))?;
        if !crate::workflow::host::router_credential(&name) && !launch.env.contains_key(&name) {
            launch.env.insert(
                name,
                json!(value.to_str().context("invalid worker environment")?),
            );
        }
    }
    call(
        state,
        row,
        json!({"op":"create","unit":row["dispatch"],"attempt":row["attempt"],
        "cwd":row["cwd"],"argv":launch.argv,"env":launch.env}),
    )
}

fn binding(state: &Path, row: &Value, intent: &Path) -> Result<(Value, bool)> {
    let mut ticket = row["create"]["ticket"].clone();
    let closing = match fs::File::open(intent) {
        Ok(file) => {
            let saved: Value = serde_json::from_reader(file)?;
            ensure!(
                saved.is_object() && (ticket.is_null() || saved == ticket),
                "bundled close intent does not match assignment"
            );
            ticket = saved;
            true
        }
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if ticket.is_null() {
        // The publisher may have lost create's response. Only lookup of this
        // assignment can recover the binding. Never repeat create.
        let lookup = crate::bundled::lookup_assignment(
            state,
            row["dispatch"].as_str().context("missing dispatch")?,
            &row["attempt"],
        )?;
        ensure!(
            lookup["result"] == "accepted",
            "bundled create binding is unknown"
        );
        ticket = lookup["ticket"].clone();
    }
    ensure!(
        ticket.is_object()
            && ticket["unit"] == row["dispatch"]
            && ticket["attempt"] == row["attempt"],
        "bundled ticket does not match assignment"
    );
    Ok((ticket, closing))
}

fn unknown() -> Value {
    json!({"result":"uncertain","cleanup":"unknown"})
}

fn evidence(answer: &Value) -> Value {
    // Inspect also returns the worker's terminal contents. Expose only the
    // lifecycle fields, even when the terminal happens to contain credentials.
    let mut view = json!({});
    for key in [
        "result",
        "diagnostic",
        "scope",
        "ticket",
        "cleanup",
        "proof",
        "execution_cleanup",
        "execution_proof",
        "reconcile",
        "category",
    ] {
        if let Some(value) = answer.get(key) {
            view[key] = value.clone();
        }
    }
    if view["result"] == "completed" && view["proof"]["proves"] == "execution-empty" {
        view["cleanup"] = json!("confirmed");
    }
    view
}

pub(super) fn observe(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let mut answer = unknown();
    let path = closed_path(state, row)?.with_extension("closing");
    if let Ok((ticket, closing)) = binding(state, row, &path) {
        let op = if closing {
            "cleanup-receipt"
        } else {
            "inspect"
        };
        answer = crate::bundled::assignment_operation(state, &ticket, op).0;
        answer["ticket"] = ticket;
    }
    let mut view = evidence(&answer);
    let terminal = answer.get("execution_proof").unwrap_or(&answer["proof"]);
    let live = answer["result"] == "accepted"
        && answer["identity"]["worker"].is_object()
        && answer["data"]["exit"].is_null()
        && answer["data"]["read_error"].is_null();
    let worker = match terminal["state"].as_str() {
        Some("closed") => json!({"state":"cancelled","exit_code":null,"signal":null}),
        Some("exited" | "faulted") => json!({"state":"exited","exit_code":null,"signal":null}),
        _ if live => json!({"state":"running"}),
        _ => json!({"state":"unknown"}),
    };
    if answer["scope"] != "execution" {
        if live {
            view["alive"] = json!(true);
            view["owned"] = json!(true);
        } else if view["cleanup"] == "confirmed" {
            view["alive"] = json!(false);
        }
    }
    Ok((worker, view))
}

pub(super) fn close(state: &Path, row: &Value) -> Result<Value> {
    let path = closed_path(state, row)?.with_extension("closing");
    let answer = match binding(state, row, &path) {
        Err(error) => {
            json!({"result":"uncertain","cleanup":"unknown","diagnostic":error.to_string()})
        }
        Ok((ticket, true)) => {
            crate::bundled::assignment_operation(state, &ticket, "cleanup-receipt").0
        }
        Ok((ticket, false)) => {
            // Parent holds the assignment lock. Sync the intent before any
            // frame can leave, including if the public CLI dies mid-call.
            write_json(&path, &ticket, None)?;
            fs::File::open(path.parent().context("missing dispatch directory")?)?.sync_all()?;
            let (answer, sent) = crate::bundled::assignment_operation(state, &ticket, "close");
            if (!sent
                && answer["result"] != "completed"
                && answer["execution_cleanup"] != "confirmed")
                || answer["result"] == "refused"
            {
                fs::remove_file(path)?;
            }
            answer
        }
    };
    ensure!(
        answer["result"] != "refused",
        "close refused: {}",
        answer["diagnostic"]
    );
    let mut answer = evidence(&answer);
    answer["operation"] = json!("close");
    for key in ["dispatch", "round", "attempt"] {
        answer[key] = row[key].clone();
    }
    Ok(answer)
}
