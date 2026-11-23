//! Fence the current writer durably before asking its native backend to interrupt.
use super::{assignment, backend, controller, messages, record_scope, state_dir, view, write_json};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::File,
    path::Path,
    thread::sleep,
    time::{Duration, Instant},
};

pub const USAGE: &str = "wfc interrupt DISPATCH [--owner-token TOKEN]";

pub struct Interrupt<'a> {
    dispatch: &'a str,
    owner_token: Option<&'a str>,
}

impl<'a> Interrupt<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut id = None;
        let [owner_token] =
            messages::options(args, ["--owner-token"], Some((&mut id, "dispatch ID")))?;
        let dispatch = id
            .filter(|id| super::valid_id(id))
            .context("invalid or missing dispatch ID")?;
        Ok(Self {
            dispatch,
            owner_token,
        })
    }

    pub fn run(self) -> Result<Value> {
        let state = state_dir()?;
        let dir = state.join("dispatches");
        let path = dir.join(format!("{}.json", self.dispatch));
        let read = || {
            assignment::public_record(&dir, self.dispatch)?
                .with_context(|| format!("unknown native dispatch {}", self.dispatch))
        };
        let initial = read()?;
        let scope = record_scope(&initial).context("invalid dispatch scope")?;
        let token = controller::token(self.owner_token);
        controller::Owners::read(&state)?.require(scope, token.as_deref(), "interrupt")?;
        ensure!(
            verified_assignment(&initial),
            "interrupt requires a confirmed native tmux worker assignment"
        );
        let (_scope, _execution) = lock_execution(&state, &initial, scope, token.as_deref())?;
        let row = read()?;
        ensure!(
            record_scope(&row) == Some(scope)
                && ["round", "attempt", "executionId"]
                    .iter()
                    .all(|key| row[*key] == initial[*key]),
            "assignment changed while waiting to interrupt; inspect status and retry"
        );
        if let Some(event) =
            assignment::unresolved_lifecycle(&row).or_else(|| assignment::interrupt_fence(&row))
        {
            return Err(controller::ControllerError::from_detail(json!({
                "error":"lifecycle_reconcile_required","dispatch":self.dispatch,
                "operation":event["operation"],"audit":event["id"],"result":event["result"],
                "round":row["round"],"attempt":row["attempt"],
                "diagnostic":"previous lifecycle operation or interrupt fence requires explicit follow-up"
            })).into());
        }
        ensure!(
            row == initial,
            "assignment changed while preparing interrupt"
        );
        let (worker, inspected) = backend::observe(&state, &mut row.clone())?;
        let launch = inspected["launchId"].as_str();
        ensure!(
            inspected["result"] == "accepted"
                && inspected["alive"] == true
                && inspected["owned"] == true
                && inspected["workerBound"] == true
                && inspected["executionId"] == row["executionId"]
                && inspected["receipt"] == row["create"]["receipt"]
                && launch.is_some()
                && worker["state"] == "running",
            "interrupt requires the live original tmux writer; inspect status"
        );
        let launch = launch.context("missing verified tmux launch")?;

        let (mut next, index) = {
            let _ledger = messages::lock_for_publish(&state)?;
            let owners = controller::Owners::read(&state)?;
            owners.require(scope, token.as_deref(), "interrupt")?;
            let generation = owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone();
            ensure!(
                read()? == row,
                "assignment changed while preparing interrupt"
            );
            let history = view::merged_history(&row, messages::history(&state, self.dispatch)?)?;
            let mut projected = row.clone();
            view::project(&mut projected, &history, Some(true), false)?;

            let mut next = row.clone();
            next["rounds"] = projected["rounds"].clone();
            let current = next["rounds"]
                .as_array_mut()
                .context("missing round history")?
                .iter_mut()
                .find(|entry| entry["round"] == row["round"] && entry["attempt"] == row["attempt"])
                .context("missing current round")?;
            if !matches!(
                current["state"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                current["state"] = json!("unknown");
            }
            if next["audit"].is_null() {
                next["audit"] = json!([]);
            }
            let audit = next["audit"]
                .as_array_mut()
                .context("invalid lifecycle audit")?;
            let index = audit.len();
            audit.push(json!({
                "id":index + 1, "operation":"interrupt", "dispatch":row["dispatch"],
                "round":row["round"], "attempt":row["attempt"], "fromAttempt":row["attempt"],
                "fromHost":row["host"], "toHost":row["host"], "ownerGeneration":generation,
                "scopeFence":true, "at":chrono::Utc::now().to_rfc3339(), "result":"pending",
                "target":{"backend":row["backend"],"executionId":row["executionId"],
                    "tmuxSocket":row["tmuxSocket"],"launchId":launch,
                    "receipt":inspected["receipt"]}
            }));
            write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
            (next, index)
        };

        // The backend rechecks the live pane and process in its guarded
        // command. From the first send, missing confirmation is ambiguous.
        let outcome = backend::interrupt(&state, &row, launch).unwrap_or_else(
            |error| json!({"result":"uncertain","diagnostic":format!("{error:#}")}),
        );
        let result = match (outcome["result"].as_str(), outcome["inputResult"].as_str()) {
            (Some("accepted"), _) => "accepted",
            (Some("refused"), _) => "refused",
            (Some("definitely-not-sent"), _) | (_, Some("definitely-not-sent")) => {
                "definitely-not-sent"
            }
            _ => "uncertain",
        };
        let mut response = json!({
            "dispatch":row["dispatch"],"operation":"interrupt","round":row["round"],
            "attempt":row["attempt"],"audit":index + 1,"result":result
        });
        let _ledger = messages::lock_for_publish(&state)?;
        ensure!(
            read()? == next,
            "assignment changed before interrupt settlement"
        );
        let event = &mut next["audit"][index];
        event["result"] = json!(result);
        event["settledAt"] = json!(chrono::Utc::now().to_rfc3339());
        if let Some(diagnostic) = outcome["diagnostic"].as_str() {
            event["diagnostic"] = json!(diagnostic);
            response["diagnostic"] = json!(diagnostic);
        }
        write_json(&path, &next, None)?;
        messages::sync_directory(&dir)?;
        Ok(response)
    }
}

fn verified_assignment(row: &Value) -> bool {
    row["backend"] == "tmux"
        && row["host"].is_string()
        && assignment::identified(row)
        && row["controllerOwner"].is_null()
        && row["createPending"] != true
        && row["executionId"] == row["attempt"]
        && row["tmuxSocket"]
            .as_str()
            .is_some_and(|name| Path::new(name).is_absolute())
        && row["create"]["result"] == "accepted"
        && row["create"]["receipt"].is_string()
}

fn lock_execution(
    state: &Path,
    row: &Value,
    scope: &str,
    token: Option<&str>,
) -> Result<(File, File)> {
    let start = Instant::now();
    loop {
        // Do not prevent another controller taking over while the execution
        // lock is busy. The next pass must authorize the current generation.
        if let Some(scope_lock) = controller::try_scope_lock(state, scope)? {
            controller::Owners::read(state)?.require(scope, token, "interrupt")?;
            if let Some(execution_lock) = assignment::try_attempt_lock(state, row)? {
                return Ok((scope_lock, execution_lock));
            }
        }
        let remaining = Duration::from_secs(30).saturating_sub(start.elapsed());
        ensure!(!remaining.is_zero(), "interrupt execution lock timed out");
        sleep(remaining.min(Duration::from_millis(50)));
    }
}
