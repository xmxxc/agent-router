//! Settle a lifecycle audit without launching a worker or submitting input.
use super::{assignment, backend, controller, messages, view};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsString;

pub const USAGE: &str =
    "wfc reconcile DISPATCH AUDIT --result {accepted,definitely-not-sent,unknown,refused} --actor NAME --evidence TEXT [--owner-token TOKEN]";

pub struct Reconcile<'a> {
    dispatch: &'a str,
    audit: u64,
    result: &'a str,
    actor: &'a str,
    evidence: &'a str,
    owner_token: Option<&'a str>,
}

impl<'a> Reconcile<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let [dispatch, audit, tail @ ..] = args else {
            anyhow::bail!("expected dispatch and audit");
        };
        let dispatch = dispatch
            .to_str()
            .filter(|id| super::valid_id(id))
            .context("invalid dispatch ID")?;
        let audit = audit
            .to_str()
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .context("invalid audit ID")?;
        ensure!(
            !tail.iter().any(|arg| arg == "--cleanup-receipt"),
            "--cleanup-receipt requires an integrated bundled create adapter"
        );
        let [result, actor, evidence, owner_token] = messages::options(
            tail,
            ["--result", "--actor", "--evidence", "--owner-token"],
            None,
        )?;
        let result = result
            .filter(|s| {
                matches!(
                    *s,
                    "accepted" | "definitely-not-sent" | "unknown" | "refused"
                )
            })
            .context("invalid or missing --result")?;
        let actor = actor
            .filter(|s| !s.trim().is_empty())
            .context("missing --actor")?;
        let evidence = evidence
            .filter(|s| !s.trim().is_empty())
            .context("missing --evidence")?;
        Ok(Self {
            dispatch,
            audit,
            result,
            actor,
            evidence,
            owner_token,
        })
    }

    pub fn run(self) -> Result<Value> {
        controller::require_controller("reconcile")?;
        let state = super::state_dir()?;
        let dir = state.join("dispatches");
        let read = || {
            assignment::public_record(&dir, self.dispatch)?
                .with_context(|| format!("unknown native dispatch {}", self.dispatch))
        };
        let initial = read()?;
        let scope = super::record_scope(&initial).context("invalid dispatch scope")?;
        let token = controller::token(self.owner_token);
        let authorize = || -> Result<Value> {
            let owners = controller::Owners::read(&state)?;
            owners.require(scope, token.as_deref(), "reconcile")?;
            Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
        };
        authorize()?;
        let _scope = controller::try_scope_lock(&state, scope)?
            .context("scope operation still in progress; retry reconciliation after it finishes")?;
        let generation = authorize()?;
        let mut row = read()?;
        ensure!(
            super::record_scope(&row) == Some(scope)
                && assignment::identified(&row)
                && row["createPending"] != true
                && row["controllerOwner"].is_null(),
            "invalid or changed assignment for reconciliation"
        );
        let index = audit_index(&row, self.audit)?;
        let event = &row["audit"][index];
        ensure!(
            row["backend"] == "tmux" && row["host"] == "codex" && event["operation"] == "resume",
            "public lifecycle reconciliation currently supports tmux Codex resume audits"
        );
        validate_resume_audit(&row, event)?;
        ensure!(
            event["ownerGeneration"]
                .as_u64()
                .is_none_or(|old| generation.as_u64().is_some_and(|current| current >= old)),
            "stale_controller_owner: reconciliation requires the current scope owner"
        );
        let response = |attempt: &Value, duplicate| {
            json!({"dispatch":self.dispatch,
            "operation":"reconcile","audit":self.audit,"attempt":attempt,"result":self.result,
            "duplicate":duplicate,"inputReplayed":false})
        };
        if !matches!(event["result"].as_str(), Some("pending" | "uncertain")) {
            ensure!(
                event["result"] == self.result
                    && event["resolution"]["actor"] == self.actor
                    && event["resolution"]["evidence"] == self.evidence,
                "lifecycle audit {} is already settled with a different result or evidence",
                self.audit
            );
            authorize()?;
            return Ok(response(&event["attempt"], true));
        }
        ensure!(
            event["resolution"].is_null()
                && event["attempt"] == row["attempt"]
                && event["round"] == row["round"]
                && event["session"] == row["hostSession"]
                && event["toHost"] == row["host"],
            "lifecycle audit is no longer the current execution"
        );
        ensure!(
            row["audit"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|audit| matches!(audit["result"].as_str(), Some("pending" | "uncertain")))
                .count()
                == 1,
            "another lifecycle audit is still unresolved"
        );
        let _attempt = assignment::close_lock(&state, &row)?;
        let facts = backend::reconcile_resume(&state, &row, event)?;
        match self.result {
            "accepted" => ensure!(
                facts.live || (facts.absent && !facts.never_started),
                "cannot confirm the resumed execution or its safe absence"
            ),
            "definitely-not-sent" => ensure!(
                facts.absent && !facts.started,
                "cannot confirm that the resumed execution did not start"
            ),
            _ => (),
        }
        // A completion can arrive while the backend is being inspected.
        // Read it under the same ledger lock used by the callback.
        let _ledger = messages::lock_for_publish(&state)?;
        ensure!(
            authorize()? == generation && read()? == row,
            "assignment or owner changed during reconciliation"
        );
        let history = view::merged_history(&row, messages::history(&state, self.dispatch)?)?;
        let mut projected = row.clone();
        view::project(&mut projected, &history, None, false)?;
        let attempt = row["attempt"].clone();
        let round = row["round"].clone();
        let now = chrono::Utc::now().to_rfc3339();
        let current = projected["rounds"]
            .as_array_mut()
            .context("missing round history")?
            .iter_mut()
            .find(|r| r["attempt"] == attempt && r["round"] == round)
            .context("missing current round")?;
        ensure!(
            self.result != "definitely-not-sent"
                || (current["state"] != "completed"
                    && !history.iter().any(|message| message["round"] == round
                        && message["attempt"] == attempt
                        && matches!(message["type"].as_str(), Some("done" | "question")))),
            "current worker already published a callback"
        );
        if !matches!(
            current["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            let accepted = self.result == "accepted";
            current["state"] = json!(if accepted { "running" } else { "unknown" });
            current["completed"] = if accepted { Value::Null } else { json!(now) };
        }
        let resolution = json!({"actor":self.actor,"evidence":self.evidence,"at":now,
            "ownerGeneration":generation,"previousResult":event["result"],
            "verifiedAbsent":self.result == "accepted" && facts.absent,
            "observation":facts.inspection});
        row["rounds"] = projected["rounds"].clone();
        row["audit"][index]["result"] = json!(self.result);
        row["audit"][index]["resolution"] = resolution;
        super::write_json(&dir.join(format!("{}.json", self.dispatch)), &row, None)?;
        messages::sync_directory(&dir)?;
        Ok(response(&attempt, false))
    }
}

fn audit_index(row: &Value, id: u64) -> Result<usize> {
    let mut matches = row["audit"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, event)| event["id"].as_u64() == Some(id));
    let (index, _) = matches
        .next()
        .with_context(|| format!("no lifecycle audit {id} for this dispatch"))?;
    ensure!(
        matches.next().is_none(),
        "duplicate lifecycle audit ID {id}"
    );
    Ok(index)
}

fn validate_resume_audit(row: &Value, event: &Value) -> Result<()> {
    let round = event["round"]
        .as_u64()
        .filter(|r| *r > 0)
        .context("invalid audit round")?;
    let valid_attempt = |value: &Value| {
        value
            .as_str()
            .is_some_and(|s| super::valid_identity(s, "a-"))
    };
    let rounds = row["rounds"].as_array().context("missing round history")?;
    ensure!(
        event["dispatch"] == row["dispatch"]
            && valid_attempt(&event["attempt"])
            && valid_attempt(&event["fromAttempt"])
            && event["attempt"] == event["toAttempt"]
            && event["fromAttempt"] != event["attempt"]
            && event["fromHost"] == "codex"
            && event["toHost"] == "codex"
            && event["session"]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
            && (event["ownerGeneration"].is_null()
                || event["ownerGeneration"].as_u64().is_some_and(|n| n > 0))
            && rounds
                .iter()
                .filter(|r| r["round"] == round && r["attempt"] == event["attempt"])
                .count()
                == 1
            && rounds.iter().any(|r| r["attempt"] == event["fromAttempt"]
                && r["round"]
                    .as_u64()
                    .is_some_and(|n| n == round || n.checked_add(1) == Some(round))),
        "invalid or foreign resume audit evidence"
    );
    Ok(())
}

pub(super) fn resume_absence_proof(row: &Value, generation: &Value) -> Option<usize> {
    let event = assignment::resume_audit(row)?;
    let index = audit_index(row, event["id"].as_u64()?).ok()?;
    let resolution = &event["resolution"];
    (event["result"] == "accepted"
        && event["session"] == row["hostSession"]
        && resolution["verifiedAbsent"] == true
        && resolution["observation"]["slot"]["result"] == "accepted"
        && resolution["observation"]["slot"]["alive"] == false
        && resolution["actor"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
        && resolution["evidence"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
        && resolution["consumedAt"].is_null()
        && resolution.get("ownerGeneration") == Some(generation)
        && validate_resume_audit(row, event).is_ok())
    .then_some(index)
}
