//! Fence the current writer durably before asking its native backend to interrupt.
use super::{
    assignment, backend, controller, messages, record_scope, state_dir, valid_id, valid_identity,
    view, write_json,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
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
        let authorize = || -> Result<Value> {
            let owners = controller::Owners::read(&state)?;
            owners.require(scope, token.as_deref(), "interrupt")?;
            Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
        };
        authorize()?;
        ensure!(
            verified_assignment(&initial),
            "interrupt requires a confirmed native tmux worker assignment"
        );
        let (scope_lock, execution_lock) =
            lock_execution(&state, &initial, scope, token.as_deref())?;
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
            return Err(assignment::lifecycle_reconcile_required(&row, event));
        }
        require_backend_settled(&state, scope)?;
        ensure!(
            row == initial,
            "assignment changed while preparing interrupt"
        );
        let mut projected = row.clone();
        let (worker, inspected) = backend::observe(&state, &mut projected)?;
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
        projected["worker"] = worker;

        let (mut next, index, generation) = {
            let _ledger = messages::lock_for_publish(&state)?;
            let generation = authorize()?;
            ensure!(
                read()? == row,
                "assignment changed while preparing interrupt"
            );
            let history = view::merged_history(&row, messages::history(&state, self.dispatch)?)?;
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
            (next, index, generation)
        };

        // The backend rechecks the live pane and process in its guarded
        // command. From the first send, missing confirmation is ambiguous.
        let outcome = backend::interrupt(&state, &row, launch, [&scope_lock, &execution_lock]);
        let result = terminal_result(&outcome).unwrap_or("uncertain");
        let mut response = json!({
            "dispatch":row["dispatch"],"operation":"interrupt","round":row["round"],
            "attempt":row["attempt"],"audit":index + 1,"result":result
        });
        let _ledger = messages::lock_for_publish(&state)?;
        ensure!(
            authorize()? == generation,
            "controller generation changed before interrupt settlement"
        );
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

fn order_path(state: &Path, scope: &str) -> PathBuf {
    state.join(format!(
        "interrupt-order-{}.json",
        messages::body_hash(scope)
    ))
}

fn receipt_path(state: &Path, order: &Value) -> Result<PathBuf> {
    let dispatch = order["dispatch"]
        .as_str()
        .filter(|id| valid_id(id))
        .context("invalid interrupt order dispatch")?;
    let attempt = order["attempt"]
        .as_str()
        .filter(|id| valid_identity(id, "a-"))
        .context("invalid interrupt order attempt")?;
    let audit = order["audit"]
        .as_u64()
        .filter(|&id| id > 0)
        .context("invalid interrupt order audit")?;
    Ok(state
        .join("dispatches")
        .join(format!("{dispatch}.{attempt}.{audit}.interrupt-order")))
}

fn read_order(path: &Path) -> Result<Option<Value>> {
    match File::open(path) {
        Ok(file) => Ok(Some(
            serde_json::from_reader(file).context("read interrupt order evidence")?,
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("open interrupt order evidence"),
    }
}

fn terminal_result(reply: &Value) -> Option<&str> {
    match (reply["result"].as_str(), reply["inputResult"].as_str()) {
        (result @ Some("accepted" | "refused" | "definitely-not-sent"), _) => result,
        (_, Some("definitely-not-sent")) => Some("definitely-not-sent"),
        _ => None,
    }
}

// The caller holds the scope lock. A finished client or a lost pipe is not
// proof that tmux can no longer act on an already connected request.
pub(super) fn require_backend_settled(state: &Path, scope: &str) -> Result<()> {
    let Some(order) = read_order(&order_path(state, scope))? else {
        return Ok(());
    };
    ensure!(order["scope"] == scope, "interrupt order scope mismatch");
    if read_order(&receipt_path(state, &order)?)?.is_some_and(|receipt| {
        receipt["order"] == order
            && matches!(
                receipt["result"].as_str(),
                Some("accepted" | "refused" | "definitely-not-sent")
            )
    }) {
        return Ok(());
    }
    Err(controller::ControllerError::from_detail(json!({
        "error":"lifecycle_reconcile_required", "operation":"interrupt", "scope":scope,
        "dispatch":order["dispatch"], "round":order["round"], "attempt":order["attempt"],
        "audit":order["audit"], "result":"pending", "requiresDecision":true,
        "diagnostic":"original tmux interrupt has no verified ordering receipt"
    }))
    .into())
}

pub(crate) struct BackendOrder {
    path: PathBuf,
    receipt: PathBuf,
    binding: Value,
    published: bool,
}

impl BackendOrder {
    // Prepare under the two inherited locks. Failed preflight checks do not
    // need a settlement receipt because no key can have reached tmux.
    pub(crate) fn begin(
        state: &Path,
        dispatch: &str,
        execution: &str,
        launch: &str,
    ) -> Result<Self> {
        ensure!(valid_id(dispatch), "invalid interrupt order dispatch");
        let row = assignment::public_record(&state.join("dispatches"), dispatch)?
            .context("missing interrupt order assignment")?;
        let scope = record_scope(&row).context("invalid interrupt order scope")?;
        let event = assignment::unresolved_interrupt(&row)
            .filter(|event| event["result"] == "pending")
            .context("missing pending interrupt order")?;
        let target = &event["target"];
        ensure!(
            verified_assignment(&row)
                && row["executionId"] == execution
                && target["executionId"] == execution
                && target["backend"] == row["backend"]
                && target["launchId"] == launch
                && target["receipt"] == row["create"]["receipt"]
                && target["tmuxSocket"] == row["tmuxSocket"]
                && row["tmuxSocket"].as_str().is_some_and(|socket| {
                    env::var_os("WFC_TMUX_SOCKET").as_deref() == Some(OsStr::new(socket))
                }),
            "interrupt order does not match its physical execution"
        );
        require_backend_settled(state, scope)?;
        let binding = json!({
            "scope":scope, "dispatch":dispatch, "round":row["round"], "attempt":row["attempt"],
            "audit":event["id"], "ownerGeneration":event["ownerGeneration"], "target":target
        });
        let receipt = receipt_path(state, &binding)?;
        ensure!(
            !receipt.try_exists()?,
            "interrupt order receipt already exists"
        );
        Ok(Self {
            path: order_path(state, scope),
            receipt,
            binding,
            published: false,
        })
    }

    pub(crate) fn publish(&mut self, state: &Path, server: Value) -> Result<()> {
        let scope = self.binding["scope"]
            .as_str()
            .context("missing interrupt order scope")?;
        let owner = controller::Owners::read(state)?
            .public(scope)?
            .unwrap_or(Value::Null);
        ensure!(
            owner["generation"] == self.binding["ownerGeneration"]
                && (owner.is_null() || owner["active"] == true),
            "interrupt order no longer has the original owner"
        );
        self.binding["server"] = server;
        write_json(&self.path, &self.binding, None)?;
        self.published = true;
        messages::sync_directory(state)
    }

    pub(crate) fn finish(self, mut reply: Value) -> Result<Value> {
        if let Some(result) = terminal_result(&reply).filter(|_| self.published) {
            write_json(
                &self.receipt,
                &json!({"order":self.binding, "result":result}),
                None,
            )?;
            messages::sync_directory(
                self.receipt
                    .parent()
                    .context("missing interrupt receipt directory")?,
            )?;
        } else if !self.published
            && reply["result"] == "uncertain"
            && reply["inputResult"].is_null()
        {
            // Only this live backend knows it returned before the ordering
            // record was published. Reaching the key command requires it.
            reply["inputResult"] = json!("definitely-not-sent");
        }
        Ok(reply)
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
