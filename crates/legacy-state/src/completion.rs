//! Reconcile the durable snapshot and its completion receipt before returning a writer.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::recovery::{agree, text};

struct Done {
    raw: String,
    body: Value,
    created: Option<f64>,
}

fn has_git_result(body: &Value) -> bool {
    body["report"].as_str().is_some_and(|p| !p.is_empty())
        && match (body.get("head"), body.get("dirty")) {
            (Some(Value::Null), Some(Value::Null)) => true,
            (Some(Value::String(head)), Some(Value::Bool(_))) => !head.is_empty(),
            _ => false,
        }
}

// Both pre-attempt Python formats have a report path, and the later one
// also records a complete pair of Git fields. Neither names a writer.
pub(super) fn legacy_result(body: &Value) -> bool {
    body["report"].as_str().is_some_and(|s| !s.is_empty())
        && body.as_object().is_some_and(|m| {
            m.len() == 1
                || (m.len() == 3
                    && has_git_result(body)
                    && body["head"].as_str().is_none_or(|s| {
                        matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
                    }))
        })
}

struct Snapshot {
    rowid: i64,
    round: i64,
    attempt: String,
    state: Option<String>,
    report: Option<String>,
    result: Option<String>,
    completed: Option<f64>,
    receipt: Option<Done>,
    varying_results: bool,
    missing_report: bool,
    current: bool,
}

struct Update {
    rowid: i64,
    report: String,
    result: String,
    completed: f64,
}

pub struct Completions(Vec<Update>);

impl Completions {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn apply(self, db: &Connection) -> Result<()> {
        let mut write = db.prepare(
            "UPDATE dispatch_round SET state='completed', report=coalesce(nullif(report,''),?1),
            result=coalesce(nullif(result,''),?2), completed=coalesce(completed,?3) WHERE rowid=?4",
        )?;
        let mut fill_parent = db.prepare(
            "UPDATE dispatch AS d SET report=r.report FROM dispatch_round AS r
             WHERE r.rowid=? AND d.id=r.dispatch AND d.current_round=r.round
               AND d.current_attempt=r.attempt AND nullif(d.report,'') IS NULL",
        )?;
        for u in self.0 {
            write.execute(params![u.report, u.result, u.completed, u.rowid])?;
            fill_parent.execute([u.rowid])?;
        }
        Ok(())
    }
}

impl Snapshot {
    fn eligible(&self) -> bool {
        matches!(
            self.state.as_deref(),
            None | Some("" | "running" | "completed")
        )
    }

    fn accept_result(&mut self, result: &Value) -> Result<()> {
        ensure!(result.is_object(), "invalid completion result");
        ensure!(
            result
                .get("round")
                .is_none_or(|v| v.as_i64() == Some(self.round))
                && text(result, "attempt")?.is_none_or(|v| v == self.attempt),
            "completion result conflicts with snapshot identity"
        );
        agree(
            &mut self.report,
            text(result, "report")?,
            "completion report",
        )
    }

    fn attach(&mut self, raw: String, body: Value, mut time: Option<f64>) -> Result<()> {
        if !self.eligible() {
            // A historical unknown snapshot remains diagnostic after a later
            // attempt takes over. A terminal cancellation/failure contradicts done.
            ensure!(
                self.state.as_deref() == Some("unknown") && !self.current,
                "done conflicts with snapshot state for round {}, attempt {}",
                self.round,
                self.attempt
            );
            return Ok(());
        }
        self.accept_result(&body)?;
        if let Some(previous) = &self.receipt {
            // Python upgrades pre-attempt messages with ORDER BY id LIMIT 1.
            // Later unqualified submissions never rewrote that snapshot.
            if legacy_result(&body)
                && (legacy_result(&previous.body)
                    || (previous.body["round"].as_i64() == Some(self.round)
                        && previous.body["attempt"].as_str() == Some(self.attempt.as_str())))
            {
                return Ok(());
            }
            // Modern Python appends each fresh completion, then rewrites this one snapshot
            // in the same transaction. Numeric message ID is the commit order.
            let modern = body["round"].as_i64() == Some(self.round)
                && body["attempt"].as_str() == Some(self.attempt.as_str());
            if previous.raw != raw {
                let old = previous.body.as_object().unwrap();
                let new = body.as_object().unwrap();
                let pre_attempt = !old.contains_key("round") && !old.contains_key("attempt");
                // The earliest Python writer stored only the path. Git fields
                // first appeared together; an isolated missing field is damage.
                let before_git = pre_attempt
                    && !old.contains_key("head")
                    && !old.contains_key("dirty")
                    && previous.body["report"]
                        .as_str()
                        .is_some_and(|p| !p.is_empty());
                let compatible = old
                    .iter()
                    .all(|(k, v)| matches!(k.as_str(), "head" | "dirty") || new.get(k) == Some(v))
                    && new.keys().all(|k| {
                        old.contains_key(k)
                            || before_git && matches!(k.as_str(), "head" | "dirty")
                            || pre_attempt
                                && matches!(k.as_str(), "round" | "attempt" | "scope" | "role")
                    });
                ensure!(
                    modern
                        && (before_git || has_git_result(&previous.body))
                        && has_git_result(&body)
                        && compatible,
                    "conflicting done results"
                );
                self.varying_results = true;
            } else if !modern && self.completed.is_none() {
                agree(&mut time, previous.created, "legacy done receipt times")?;
            }
        }
        self.receipt = Some(Done {
            raw,
            body,
            created: time,
        });
        Ok(())
    }

    fn running_completion(
        &self,
        db: &Connection,
        dispatch: &str,
        result: Option<&Value>,
    ) -> Result<bool> {
        if self.state.as_deref() != Some("running") {
            return Ok(false);
        }
        let task_fields = result.is_some_and(|v| {
            ["report", "head", "dirty"]
                .iter()
                .any(|k| v.get(k).is_some())
        });
        if task_fields {
            ensure!(
                result.is_some_and(has_git_result) && self.completed.is_some(),
                "running snapshot has incomplete completion evidence"
            );
            ensure!(
                !result
                    .is_some_and(|v| ["backend", "diagnostic"].iter().any(|k| v.get(k).is_some())),
                "running snapshot mixes task completion and backend diagnostics"
            );
            return Ok(true);
        }

        let Some(observed) = self.completed else {
            return Ok(false);
        };
        // A failed backend inspection also timestamps this column. A later
        // accepted reconciliation restores running on this same attempt.
        let backend_only = result.is_none_or(|v| {
            ["backend", "result", "diagnostic"]
                .iter()
                .all(|k| v[*k].as_str().is_some_and(|s| !s.is_empty()))
        });
        let reconciled = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM lifecycle_event WHERE dispatch=?1 AND round=?2
                AND coalesce(to_attempt,attempt)=?3 AND operation IN ('create','resume','handoff')
                AND result='accepted' AND resolver_actor<>'' AND resolution_evidence<>'' AND resolved_at>=?4)",
            params![dispatch, self.round, self.attempt, observed], |r| r.get::<_, bool>(0),
        )?;
        ensure!(
            backend_only && reconciled,
            "running snapshot has incomplete completion evidence"
        );
        Ok(false)
    }

    fn update(self, db: &Connection, dispatch: &str) -> Result<Option<Update>> {
        if !self.eligible() {
            return Ok(None);
        }
        let completed = self.state.as_deref() == Some("completed");
        let stored: Option<Value> = self
            .result
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        if let Some(receipt) = &self.receipt {
            if legacy_result(&receipt.body) {
                ensure!(
                    stored.as_ref().is_none_or(|v| v == &receipt.body)
                        && self
                            .completed
                            .zip(receipt.created)
                            .is_none_or(|(saved, first)| saved == first),
                    "stored snapshot conflicts with first legacy done receipt"
                );
            }
            if self.varying_results {
                ensure!(
                    stored.as_ref().is_some_and(has_git_result),
                    "conflicting done results without a final snapshot result"
                );
            }
            ensure!(
                !stored
                    .as_ref()
                    .is_some_and(|v| v.get("backend").is_some() || v.get("diagnostic").is_some()),
                "stored backend diagnostic conflicts with done receipt"
            );
            if let (Some(saved_time), Some(observed)) = (self.completed, receipt.created) {
                ensure!(
                    saved_time >= observed,
                    "stored completion time precedes the final done receipt"
                );
            }
            if let Some(original) = &stored {
                let fields = original
                    .as_object()
                    .context("invalid stored completion result")?;
                ensure!(
                    fields
                        .iter()
                        .all(|(k, v)| receipt.body.get(k).is_none_or(|other| other == v)),
                    "conflicting stored result and final done receipt"
                );
            }
        } else if !completed && !self.running_completion(db, dispatch, stored.as_ref())? {
            return Ok(None);
        }
        let needs_update =
            !completed || self.missing_report || self.result.is_none() || self.completed.is_none();
        let report = self.report.context("missing completed report")?;
        let receipt_time = self.receipt.as_ref().and_then(|r| r.created);
        let result = self
            .result
            .or_else(|| self.receipt.map(|r| r.raw))
            .context("missing completed result")?;
        // Python samples the snapshot clock separately. If that value is lost,
        // the matched receipt time is the only durable completion observation.
        let completed = self
            .completed
            .or(receipt_time)
            .context("missing completion time evidence")?;
        Ok(needs_update.then_some(Update {
            rowid: self.rowid,
            report,
            result,
            completed,
        }))
    }
}

// A round may contain multiple attempts after a resume. Prefer an exact
// identity; unqualified older reports may survive solely as historical messages.
fn attach_receipt(
    rounds: &mut [Snapshot],
    raw: String,
    time: Option<f64>,
    recipient: Option<String>,
) -> Result<()> {
    let body: Value = serde_json::from_str(&raw).context("unreadable done receipt")?;
    ensure!(body.is_object(), "invalid done receipt");
    let attempt = text(&body, "attempt")?;
    let report = text(&body, "report")?;
    let round = body
        .get("round")
        .filter(|v| !v.is_null())
        .map(|v| v.as_i64().filter(|n| *n > 0).context("invalid done round"))
        .transpose()?;
    let qualified = round.is_some() || attempt.is_some();
    let identities = rounds.iter().enumerate().filter(|(_, s)| {
        round.is_none_or(|n| n == s.round) && attempt.as_ref().is_none_or(|a| a == &s.attempt)
    });
    let mut candidates = identities.clone();
    let target = match (qualified, candidates.next(), candidates.next()) {
        (true, Some((i, _)), None) => Some(i),
        _ => {
            let mut same_report =
                identities.filter(|(_, s)| report.is_some() && report == s.report);
            match (same_report.next(), same_report.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        }
    };
    if let Some(i) = target {
        ensure!(
            recipient.as_deref().is_none_or(|v| v == "controller"),
            "unexpected done recipient"
        );
        rounds[i].attach(raw, body, time)?;
    } else {
        ensure!(!qualified, "done has no unique attempt snapshot");
    }
    Ok(())
}

pub fn plan(db: &Connection) -> Result<Completions> {
    let mut groups: BTreeMap<String, Vec<Snapshot>> = BTreeMap::new();
    let mut q = db.prepare(
        "WITH named_reports AS (
            SELECT dispatch,body->>'$.round' n,body->>'$.attempt' a,min(body->>'$.report') path
            FROM message WHERE type='done' AND json_valid(body) AND json_type(body,'$.report')='text'
            GROUP BY dispatch,n,a HAVING count(DISTINCT body->>'$.report')=1)
        SELECT r.dispatch,r.rowid,r.round,r.attempt,r.state,nullif(r.report,''),nullif(r.result,''),r.completed,
        CASE WHEN d.current_round=r.round AND d.current_attempt=r.attempt THEN nullif(d.report,'') END,
        d.current_round IS r.round AND d.current_attempt IS r.attempt AND d.id IS NOT NULL,nullif(m.path,'')
        FROM dispatch_round r LEFT JOIN dispatch d ON d.id=r.dispatch
        LEFT JOIN named_reports m ON m.dispatch=r.dispatch AND m.n=r.round AND m.a=r.attempt ORDER BY r.dispatch,r.round,r.rowid",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let mut report: Option<String> = r.get(5)?;
        let missing_report = report.is_none();
        agree(&mut report, r.get(8)?, "current snapshot report")
            .with_context(|| format!("dispatch {id}"))?;
        let mut snapshot = Snapshot {
            rowid: r.get(1)?,
            round: r
                .get(2)
                .with_context(|| format!("missing snapshot round for dispatch {id}"))?,
            attempt: r
                .get(3)
                .with_context(|| format!("missing snapshot attempt for dispatch {id}"))?,
            state: r.get(4)?,
            report,
            result: r.get(6)?,
            completed: r.get(7)?,
            receipt: None,
            varying_results: false,
            missing_report,
            current: r.get(9)?,
        };
        if snapshot.eligible() {
            // Identify a missing path before replaying older path-only receipts.
            // A later modern done may be the surviving witness to its identity.
            if snapshot.report.is_none() {
                snapshot.report = r.get(10)?;
            }
            if let Some(raw) = &snapshot.result {
                let result = serde_json::from_str(raw)
                    .with_context(|| format!("invalid recorded result for dispatch {id}"))?;
                snapshot
                    .accept_result(&result)
                    .with_context(|| format!("dispatch {id}"))?;
            }
        }
        groups.entry(id).or_default().push(snapshot);
    }
    // Scan the message table once even when many dispatches share this ledger.
    let mut receipts = db.prepare(
        "SELECT dispatch,body,created,recipient,nullif(request_key,'') FROM message WHERE type='done' ORDER BY id",
    )?;
    let mut messages = receipts.query([])?;
    let mut request_keys = BTreeSet::new();
    while let Some(row) = messages.next()? {
        let id: Option<String> = row.get(0)?;
        let Some(id) = id else { continue };
        if let Some(rounds) = groups.get_mut(&id) {
            // A keyed retry returns the existing receipt in Python. Only a fresh
            // submission can append another message to the completion history.
            if let Some(key) = row.get::<_, Option<String>>(4)? {
                ensure!(
                    request_keys.insert((id.clone(), key)),
                    "dispatch {id} has duplicate done request keys"
                );
            }
            let mut check = || attach_receipt(rounds, row.get(1)?, row.get(2)?, row.get(3)?);
            check().with_context(|| format!("cannot reconcile completions for dispatch {id}"))?;
        }
    }
    let mut updates = Vec::new();
    for (id, rounds) in groups {
        for snapshot in rounds {
            let round = snapshot.round;
            if let Some(update) = snapshot
                .update(db, &id)
                .with_context(|| format!("dispatch {id}, round {round}"))?
            {
                updates.push(update);
            }
        }
    }
    Ok(Completions(updates))
}
