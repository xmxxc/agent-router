use super::{
    assignment, controller, legacy_status_state, messages, observe_dispatch, project_dispatch,
    record_scope, state_dir, valid_id, view, worker_owner,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::ExitCode;

pub const USAGE: &str = "wfc reclaim [--dispatch ID ...] [--yes] [--owner-token TOKEN]";

pub struct Reclaim<'a> {
    dispatches: Vec<&'a str>,
    yes: bool,
    owner_token: Option<&'a str>,
}

impl<'a> Reclaim<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut request = Self {
            dispatches: Vec::new(),
            yes: false,
            owner_token: None,
        };
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            match flag.to_str() {
                Some("--yes") => request.yes = true,
                Some("--dispatch") => {
                    let id = args
                        .next()
                        .and_then(|arg| arg.to_str())
                        .filter(|id| !id.is_empty() && !id.starts_with('-'))
                        .context("--dispatch requires an ID")?;
                    if !request.dispatches.contains(&id) {
                        request.dispatches.push(id);
                    }
                }
                Some("--owner-token") => {
                    let token = args
                        .next()
                        .and_then(|arg| arg.to_str())
                        .filter(|token| !token.starts_with("--"))
                        .context("--owner-token requires a value")?;
                    ensure!(
                        request.owner_token.replace(token).is_none(),
                        "duplicate --owner-token"
                    );
                }
                _ => bail!("unknown reclaim option"),
            }
        }
        Ok(request)
    }

    pub fn run(self) -> Result<ExitCode> {
        if let Some(state) = legacy_status_state(self.dispatches.first().map(OsStr::new))? {
            if self.yes {
                eprintln!("wfc: reclaim supports only read-only preview of Python history");
                return Ok(ExitCode::from(2));
            }
            return self.legacy_preview(&state);
        }

        let state = state_dir()?;
        let dir = state.join("dispatches");
        let named = !self.dispatches.is_empty();
        let mut targets = Vec::new();
        if named {
            // Resolve the entire selection before the first close.
            for id in &self.dispatches {
                ensure!(valid_id(id), "unknown dispatch {id} in {}", state.display());
                let row = assignment::public_record(&dir, id)?
                    .with_context(|| format!("unknown dispatch {id} in {}", state.display()))?;
                targets.push((row, None));
            }
        } else {
            let ids = assignment::ids(&dir)?;
            if ids.is_empty() && !self.yes {
                let legacy = wfc_legacy_state::state_dir()?;
                if !legacy.join("router.db").try_exists()? {
                    return self.legacy_preview(&legacy);
                }
            }
            for id in ids {
                if let Some(row) = assignment::public_record(&dir, &id)? {
                    if !completed_task(&state, &row)? {
                        continue;
                    }
                    let (item, default_target) = inspect(&state, &dir, &row)?;
                    if default_target {
                        targets.push((row, Some(item)));
                    }
                }
            }
            targets.sort_by(|(a, _), (b, _)| {
                a["created"]
                    .as_f64()
                    .unwrap_or_default()
                    .total_cmp(&b["created"].as_f64().unwrap_or_default())
                    .then_with(|| a["dispatch"].as_str().cmp(&b["dispatch"].as_str()))
            });
        }

        let token = controller::token(self.owner_token);
        let mut sessions = Vec::new();
        for (selected, preview) in targets {
            let scope = record_scope(&selected).context("invalid dispatch scope")?;
            authorize(&state, scope, token.as_deref())?;
            let outcome = if self.yes {
                confirm(&state, &dir, &selected, scope, token.as_deref(), named)
            } else if let Some(preview) = preview {
                Ok(preview)
            } else {
                inspect(&state, &dir, &selected).map(|(item, _)| item)
            };
            match outcome {
                Ok(item) => sessions.push(item),
                Err(error) if error.is::<controller::ControllerError>() => return Err(error),
                Err(error) => {
                    let mut item = session(&selected);
                    failure(&mut item, "failed", format!("{error:#}"));
                    sessions.push(item);
                }
            }
        }

        println!(
            "{}",
            json!({"state":state,"yes":self.yes,"sessions":sessions})
        );
        let mut failed = false;
        if self.yes {
            for item in &sessions {
                if let Some(error) = item["error"].as_str() {
                    eprintln!("wfc: {error}");
                    failed = true;
                }
            }
        }
        Ok(if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        })
    }

    fn legacy_preview(&self, state: &Path) -> Result<ExitCode> {
        let mut preview =
            wfc_legacy_state::reclaim_preview(state, self.dispatches.first().copied())?;
        for id in self.dispatches.iter().skip(1) {
            let mut next = wfc_legacy_state::reclaim_preview(state, Some(id))?;
            preview["sessions"]
                .as_array_mut()
                .context("invalid legacy preview")?
                .append(
                    next["sessions"]
                        .as_array_mut()
                        .context("invalid legacy preview")?,
                );
        }
        println!("{preview}");
        Ok(ExitCode::SUCCESS)
    }
}

fn authorize(state: &Path, scope: &str, token: Option<&str>) -> Result<()> {
    controller::Owners::read(state)?.require(scope, token, "reclaim")?;
    Ok(())
}

fn session(row: &Value) -> Value {
    json!({"dispatch":row["dispatch"],"label":row["label"],"tmux":row["tmux"],
        "state":row["state"].as_str().unwrap_or("unknown"),"reclaimable":false})
}

fn failure(item: &mut Value, result: &str, error: impl AsRef<str>) {
    item["result"] = json!(result);
    item["error"] = json!(format!(
        "{} {}",
        item["dispatch"].as_str().unwrap_or_default(),
        error.as_ref()
    ));
}

fn gone(item: &mut Value) {
    item["result"] = json!("gone");
    item["note"] = json!("session or worker no longer exists; skipped");
    item["reclaimable"] = json!(false);
}

fn completed_task(state: &Path, row: &Value) -> Result<bool> {
    let mut projected = row.clone();
    if projected["host"].is_string() {
        projected["worker"] = json!({"state":"unknown"});
    }
    project_history(state, &mut projected)?;
    Ok(projected["state"] == "done" && projected["reclaimed"].is_null())
}

fn project_history(state: &Path, row: &mut Value) -> Result<()> {
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    let history = view::merged_history(row, messages::history(state, id)?)?;
    project_dispatch(row, &history, false)
}

fn inspect(state: &Path, dir: &Path, row: &Value) -> Result<(Value, bool)> {
    let mut current = row.clone();
    let observation = observe_dispatch(state, dir, &mut current, true)?;
    project_history(state, &mut current)?;

    let mut item = session(&current);
    if !same_execution(row, &current) {
        item["result"] = json!("changed");
        item["note"] = json!("dispatch execution changed; skipped");
        return Ok((item, false));
    }
    let result = observation["result"].as_str().unwrap_or("uncertain");
    let diagnostic = observation["diagnostic"].as_str().unwrap_or(result);
    let close_pending =
        current["closePending"] == true || current["resourceCleanup"] == "uncertain";
    let default_target = current["state"] == "done"
        && current["reclaimed"].is_null()
        && !close_pending
        && result == "accepted"
        && observation["alive"] == true;
    item["reclaimable"] = json!(default_target);
    if close_pending {
        failure(&mut item, "failed", "close evidence is unresolved");
    } else if observation["alive"] == true && observation["owned"] == false {
        failure(&mut item, "refused", "session is not router-owned");
    } else if observation["alive"] == false && matches!(result, "accepted" | "completed") {
        gone(&mut item);
    } else if result != "accepted" || observation["alive"] != true || observation["owned"] != true {
        failure(
            &mut item,
            if result == "accepted" {
                "uncertain"
            } else {
                result
            },
            diagnostic,
        );
    } else if !current["reclaimed"].is_null() {
        item["result"] = json!("changed");
        item["note"] = json!("execution already reclaimed; skipped");
    } else {
        item["result"] = json!("listed");
        if current["state"] != "done" {
            item["note"] =
                json!("task is unfinished; close requires an explicit --dispatch and --yes");
        }
    }
    Ok((item, default_target))
}

fn same_execution(selected: &Value, current: &Value) -> bool {
    // Herdr can learn more writer identities while observing the same pane.
    let binding_fields = [
        "endpoint",
        "service",
        "workspace",
        "pane",
        "terminal",
        "nonce",
        "receipt",
    ];
    record_scope(selected) == record_scope(current)
        && [
            "dispatch",
            "round",
            "attempt",
            "executionId",
            "host",
            "backend",
            "tmux",
            "tmuxSocket",
            "legacyExecutionId",
            "legacyLaunchId",
            "legacyWorkerPane",
            "endpoint",
            "cwd",
            "createPending",
        ]
        .iter()
        .all(|&key| selected[key] == current[key])
        && selected["create"]["receipt"] == current["create"]["receipt"]
        && binding_fields
            .iter()
            .all(|&key| selected["backendHandle"][key] == current["backendHandle"][key])
        && selected["backendHandle"]["writerProcesses"].get(0)
            == current["backendHandle"]["writerProcesses"].get(0)
}

fn confirm(
    state: &Path,
    dir: &Path,
    selected: &Value,
    scope: &str,
    token: Option<&str>,
    named: bool,
) -> Result<Value> {
    let _lock = controller::lock_scope(state, scope)?;
    authorize(state, scope, token)?;
    let id = selected["dispatch"].as_str().context("missing dispatch")?;
    let current = assignment::public_record(dir, id)?;
    let Some(current) = current.filter(|row| same_execution(selected, row)) else {
        let mut item = session(selected);
        item["result"] = json!("changed");
        item["note"] = json!("dispatch execution changed; skipped");
        return Ok(item);
    };

    let (mut item, default_target) = inspect(state, dir, &current)?;
    if item["result"] != "listed" {
        return Ok(item);
    }
    if !named && !default_target {
        item["result"] = json!("changed");
        item["note"] = json!("dispatch is no longer done and unreclaimed; skipped");
        return Ok(item);
    }
    let event = match worker_owner::validate_close(&current)
        .and_then(|()| messages::Reclaimed::prepare(&current))
    {
        Ok(event) => event,
        Err(error) => {
            failure(&mut item, "failed", format!("{error:#}"));
            return Ok(item);
        }
    };
    // A lease can expire while the backend is being inspected.
    authorize(state, scope, token)?;
    let closed = match worker_owner::close_locked(state, &current) {
        Ok(closed) => closed,
        Err(error) => {
            failure(&mut item, "failed", format!("{error:#}"));
            return Ok(item);
        }
    };
    if closed["result"] != "completed" {
        failure(
            &mut item,
            "failed",
            format!(
                "could not close resource ({}) {}",
                closed["result"],
                closed["diagnostic"].as_str().unwrap_or_default()
            ),
        );
    } else if closed["diagnostic"] == "session was already gone"
        || closed["closeEffect"] == "already_absent"
    {
        gone(&mut item);
    } else {
        match event.publish(state) {
            Ok(id) => {
                item["result"] = json!("reclaimed");
                item["message"] = json!(id);
            }
            Err(error) => failure(&mut item, "failed", format!("resource closed, {error:#}")),
        }
    }
    Ok(item)
}
