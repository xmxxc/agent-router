//! Reconcile the durable snapshot and its completion receipt before returning a writer.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::recovery::{agree, text};

const GIT_FIELDS: [&str; 2] = ["head", "dirty"];

struct Done {
    raw: String,
    body: Value,
    created: Option<f64>,
}

// Normalize an exact JSON number to its significant digits and base-10 exponent.
fn comparison_number(token: &str) -> Result<String> {
    let (sign, unsigned) = token.strip_prefix('-').map_or(("", token), |n| ("-", n));
    let (mantissa, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{integer}{fraction}");
    let significant = digits.trim_matches('0');
    if significant.is_empty() {
        return Ok("0".into());
    }
    let trailing = digits.len() - digits.trim_end_matches('0').len();
    let offset = trailing as i128 - fraction.len() as i128;
    let exponent = exponent
        .parse::<i128>()
        .ok()
        .and_then(|n| n.checked_add(offset))
        .context("JSON number exponent exceeds comparison range")?;
    Ok(format!("{sign}{significant}e{exponent}"))
}

// The original JSON has already been parsed and validated at this point.
// Encode exact numbers and strings with distinct prefixes in a comparison-only
// value. Object keys still use serde's usual decoding and order-independent map.
fn comparison_fields(raw: &str) -> Result<Map<String, Value>> {
    let bytes = raw.as_bytes();
    let mut comparison = String::with_capacity(raw.len());
    let (mut copied, mut index) = (0, 0);
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let start = index;
                index += 1;
                while bytes[index] != b'"' {
                    index += if bytes[index] == b'\\' { 2 } else { 1 };
                }
                index += 1;
                if !raw[index..].trim_start().starts_with(':') {
                    comparison.push_str(&raw[copied..start + 1]);
                    comparison.push('s');
                    copied = start + 1;
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = index;
                while matches!(
                    bytes.get(index),
                    Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                ) {
                    index += 1;
                }
                comparison.push_str(&raw[copied..start]);
                comparison.push_str("\"n");
                comparison.push_str(&comparison_number(&raw[start..index])?);
                comparison.push('"');
                copied = index;
            }
            _ => index += 1,
        }
    }
    comparison.push_str(&raw[copied..]);
    Ok(serde_json::from_str(&comparison)?)
}

fn differing_fields(
    left: &str,
    right: &str,
) -> Result<Option<(Map<String, Value>, Map<String, Value>)>> {
    if left == right {
        return Ok(None);
    }
    let left = comparison_fields(left)?;
    let right = comparison_fields(right)?;
    Ok((left != right).then_some((left, right)))
}

fn valid_git_pair(head: Option<&Value>, dirty: Option<&Value>) -> bool {
    match (head, dirty) {
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::String(head)), Some(Value::Bool(_))) => !head.is_empty(),
        _ => false,
    }
}

fn has_git_result(body: &Value) -> bool {
    body["report"].as_str().is_some_and(|p| !p.is_empty())
        && valid_git_pair(body.get("head"), body.get("dirty"))
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

/// A later pre-attempt receipt did not rewrite the earlier snapshot evidence.
pub fn keeps_earlier_done(previous: &Value, next: &Value, round: i64, attempt: &str) -> bool {
    legacy_result(next)
        && (legacy_result(previous)
            || (previous["round"].as_i64() == Some(round)
                && previous["attempt"].as_str() == Some(attempt)))
}

/// Compare the fields present in both JSON objects without replacing evidence.
pub fn check_final_done_result(stored: &str, receipt: &str) -> Result<()> {
    let difference = differing_fields(stored, receipt)?;
    ensure!(
        difference.is_none_or(|(fields, evidence)| fields
            .iter()
            .all(|(key, value)| evidence.get(key).is_none_or(|other| other == value))),
        "conflicting stored result and final done receipt"
    );
    Ok(())
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
    previous_git: Vec<[Option<Value>; 2]>,
    modern_writer: bool,
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
            result=?2, completed=coalesce(completed,?3) WHERE rowid=?4",
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
            if keeps_earlier_done(&previous.body, &body, self.round, &self.attempt) {
                return Ok(());
            }
            // Modern Python appends each fresh completion, then rewrites this one snapshot
            // in the same transaction. Numeric message ID is the commit order.
            let modern = body["round"].as_i64() == Some(self.round)
                && body["attempt"].as_str() == Some(self.attempt.as_str());
            if let Some((old, new)) = differing_fields(&previous.raw, &raw)? {
                let pre_attempt = !old.contains_key("round") && !old.contains_key("attempt");
                // The earliest Python writer stored only the path. Git fields
                // first appeared together; an isolated missing field is damage.
                let before_git = pre_attempt
                    && !old.contains_key("head")
                    && !old.contains_key("dirty")
                    && previous.body["report"]
                        .as_str()
                        .is_some_and(|p| !p.is_empty());
                let compatible = old.iter().all(|(k, v)| {
                    matches!(k.as_str(), "report" | "head" | "dirty") || new.get(k) == Some(v)
                }) && new.keys().all(|k| {
                    old.contains_key(k)
                        || before_git && matches!(k.as_str(), "head" | "dirty")
                        || pre_attempt
                            && matches!(k.as_str(), "round" | "attempt" | "scope" | "role")
                });
                ensure!(
                    modern && (before_git || has_git_result(&previous.body)) && compatible,
                    "conflicting done results"
                );
                // Keep only the earlier Git fields, so a surviving part of the
                // snapshot can distinguish the final submission if its clock is lost.
                self.previous_git
                    .push(GIT_FIELDS.map(|k| previous.body.get(k).cloned()));
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

    fn corroborates_last_done(&self, stored: Option<&Value>, receipt: &Done) -> bool {
        let Some(stored) = stored else { return false };
        if !["report", "head", "dirty"]
            .iter()
            .any(|k| stored.get(k).is_some())
        {
            return false;
        }
        if self
            .completed
            .zip(receipt.created)
            .is_some_and(|(saved, observed)| saved >= observed)
        {
            return true;
        }
        // Every earlier result consistent with the retained fields must imply
        // the same Git outcome. Never fill a missing key from an earlier done.
        let retained = GIT_FIELDS.map(|k| stored.get(k));
        let final_git = GIT_FIELDS.map(|k| receipt.body.get(k).or_else(|| stored.get(k)));
        self.previous_git.iter().all(|old| {
            let prior = old.each_ref().map(Option::as_ref);
            prior == final_git
                || retained
                    .iter()
                    .zip(prior)
                    .any(|(v, p)| v.is_some() && *v != p)
        })
    }

    // A Python task result projects report/head/dirty out of the larger done
    // body. Keep the stored JSON when complete, including independent metadata.
    fn patch_result(&self, db: &Connection, stored: Option<&Value>) -> Result<Option<String>> {
        let receipt = self.receipt.as_ref();
        let evidence = receipt.map(|r| &r.body);
        let Some(base) = stored.or(evidence) else {
            return Ok(None);
        };

        if let Some(early) = receipt.filter(|r| legacy_result(&r.body)) {
            // Python copied the first matching legacy body verbatim.
            return Ok((base != &early.body).then(|| early.raw.clone()));
        }

        // attach_receipt already resolved any partial identity to this unique snapshot.
        let qualified = evidence.filter(|v| {
            v["round"].as_i64() == Some(self.round)
                || v["attempt"].as_str() == Some(self.attempt.as_str())
        });
        let has_git_field = |v: &Value| GIT_FIELDS.iter().any(|k| v.get(k).is_some());
        // Some receipt writers recorded round and attempt before adding Git data.
        // When a receipt survives, its own format is stronger evidence than the callback.
        let expects_git = (self.modern_writer && receipt.is_none())
            || has_git_field(base)
            || qualified.is_some_and(|v| {
                has_git_field(v) || (v.get("scope").is_some() && v.get("role").is_some())
            });
        if !expects_git {
            return Ok(None);
        }

        // SQL NULL (an absent result), a missing JSON key and a recorded JSON
        // null (a non-Git worktree) are three different observations.
        let field = |k: &str| base.get(k).or_else(|| qualified.and_then(|v| v.get(k)));
        let report = self.report.as_deref().context("missing completed report")?;
        ensure!(
            base.get("report")
                .is_none_or(|v| v.as_str() == Some(report)),
            "invalid recorded report"
        );
        let head = field("head").context("missing Git head evidence")?;
        let dirty = field("dirty").context("missing Git dirty evidence")?;
        ensure!(
            valid_git_pair(Some(head), Some(dirty)),
            "invalid recorded Git result"
        );

        if ["report", "head", "dirty"]
            .iter()
            .all(|key| base.get(key).is_some())
        {
            return Ok(None);
        }
        let raw = self
            .result
            .as_deref()
            .or_else(|| receipt.map(|r| r.raw.as_str()))
            .unwrap();
        let patched = db.query_row(
            "SELECT json_insert(?1,'$.report',?2,'$.head',json(?3),'$.dirty',json(?4))",
            params![raw, report, head.to_string(), dirty.to_string()],
            |r| r.get(0),
        )?;
        Ok(Some(patched))
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
                    stored
                        .as_ref()
                        .is_none_or(|v| v.as_object().is_some_and(|m| m
                            .iter()
                            .all(|(k, value)| receipt.body.get(k) == Some(value))))
                        && self
                            .completed
                            .zip(receipt.created)
                            .is_none_or(|(saved, first)| saved == first),
                    "stored snapshot conflicts with first legacy done receipt"
                );
            }
            if !self.previous_git.is_empty() {
                ensure!(
                    self.corroborates_last_done(stored.as_ref(), receipt),
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
            if let Some(original) = self.result.as_deref() {
                check_final_done_result(original, &receipt.raw)?;
            }
        } else if !completed && !self.running_completion(db, dispatch, stored.as_ref())? {
            return Ok(None);
        }
        let patched = self.patch_result(db, stored.as_ref())?;
        let needs_update = patched.is_some()
            || !completed
            || self.missing_report
            || self.result.is_none()
            || self.completed.is_none();
        let report = self.report.context("missing completed report")?;
        let receipt_time = self.receipt.as_ref().and_then(|r| r.created);
        let result = patched
            .or(self.result)
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
            GROUP BY dispatch,n,a HAVING count(DISTINCT body->>'$.report')=1),
        writers(dispatch,n,a) AS (
            SELECT dispatch,body->>'$.round',body->>'$.attempt' FROM message
                WHERE type='continue' AND json_valid(body)
            UNION SELECT dispatch,round,coalesce(to_attempt,attempt) FROM lifecycle_event
                WHERE operation IN ('create','resume','handoff'))
        SELECT r.dispatch,r.rowid,r.round,r.attempt,r.state,nullif(r.report,''),nullif(r.result,''),r.completed,
        CASE WHEN d.current_round=r.round AND d.current_attempt=r.attempt THEN nullif(d.report,'') END,
        d.current_round IS r.round AND d.current_attempt IS r.attempt AND d.id IS NOT NULL,nullif(m.path,''),
        (coalesce(r.callback_channel,'')<>'' AND coalesce(r.callback_hash,'')<>'') OR w.a IS NOT NULL
        FROM dispatch_round r LEFT JOIN dispatch d ON d.id=r.dispatch
        LEFT JOIN writers w ON w.dispatch=r.dispatch AND w.n=r.round AND w.a=r.attempt
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
            previous_git: Vec::new(),
            modern_writer: r.get(11)?,
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
