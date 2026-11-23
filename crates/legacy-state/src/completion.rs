//! Reconcile the durable snapshot and its completion receipt before returning a writer.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::recovery::{agree, text};

struct Snapshot {
    rowid: i64,
    round: i64,
    attempt: String,
    state: Option<String>,
    report: Option<String>,
    result: Option<String>,
    completed: Option<f64>,
    receipt: Option<String>,
    receipt_time: Option<f64>,
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
        for u in self.0 {
            write.execute(params![u.report, u.result, u.completed, u.rowid])?;
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

    fn attach(&mut self, raw: String, body: &Value, time: Option<f64>) -> Result<()> {
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
        self.accept_result(body)?;
        if let Some(stored) = &self.result {
            let original: Value = serde_json::from_str(stored)?;
            let fields = original
                .as_object()
                .context("invalid stored completion result")?;
            ensure!(
                fields
                    .iter()
                    .all(|(k, v)| body.get(k).is_none_or(|other| other == v)),
                "conflicting stored result and done receipt"
            );
        }
        agree(&mut self.receipt, Some(raw), "done results")?;
        if self.completed.is_none() {
            agree(&mut self.receipt_time, time, "done receipt times")?;
        }
        Ok(())
    }

    fn update(self) -> Result<Option<Update>> {
        let completed = self.state.as_deref() == Some("completed");
        if self.receipt.is_none() && !completed {
            return Ok(None);
        }
        let needs_update =
            !completed || self.missing_report || self.result.is_none() || self.completed.is_none();
        let report = self.report.context("missing completed report")?;
        let result = self
            .result
            .or(self.receipt)
            .context("missing completed result")?;
        // Python samples the snapshot clock separately. If that value is lost,
        // the matched receipt time is the only durable completion observation.
        let completed = self
            .completed
            .or(self.receipt_time)
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
        rounds[i].attach(raw, &body, time)?;
    } else {
        ensure!(!qualified, "done has no unique attempt snapshot");
    }
    Ok(())
}

pub fn plan(db: &Connection) -> Result<Completions> {
    let mut groups: BTreeMap<String, Vec<Snapshot>> = BTreeMap::new();
    let mut q = db.prepare(
        "SELECT r.dispatch,r.rowid,r.round,r.attempt,r.state,nullif(r.report,''),nullif(r.result,''),r.completed,
        CASE WHEN d.current_round=r.round AND d.current_attempt=r.attempt THEN nullif(d.report,'') END,
        d.current_round IS r.round AND d.current_attempt IS r.attempt AND d.id IS NOT NULL
        FROM dispatch_round r LEFT JOIN dispatch d ON d.id=r.dispatch ORDER BY r.dispatch,r.round,r.rowid",
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
            receipt_time: None,
            missing_report,
            current: r.get(9)?,
        };
        if snapshot.eligible() {
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
        "SELECT dispatch,body,created,recipient FROM message WHERE type='done' ORDER BY id",
    )?;
    let mut messages = receipts.query([])?;
    while let Some(row) = messages.next()? {
        let id: Option<String> = row.get(0)?;
        let Some(id) = id else { continue };
        if let Some(rounds) = groups.get_mut(&id) {
            let mut check = || attach_receipt(rounds, row.get(1)?, row.get(2)?, row.get(3)?);
            check().with_context(|| format!("cannot reconcile completions for dispatch {id}"))?;
        }
    }
    let mut updates = Vec::new();
    for (id, rounds) in groups {
        for snapshot in rounds {
            let round = snapshot.round;
            if let Some(update) = snapshot
                .update()
                .with_context(|| format!("dispatch {id}, round {round}"))?
            {
                updates.push(update);
            }
        }
    }
    Ok(Completions(updates))
}
