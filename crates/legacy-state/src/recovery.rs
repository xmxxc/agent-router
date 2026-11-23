//! Resolve missing dispatch identities before the pre-attempt backfill allocates any UUID.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Evidence {
    round: Option<i64>,
    attempt: Option<String>,
    report: Option<String>,
    task: Option<String>,
    // Only done supplies a terminal result. Delivery and lifecycle outcomes are
    // about messages or operations, and cannot by themselves prove task success.
    done: Option<(String, Option<f64>)>,
    started: Option<f64>,
    snapshot_row: Option<i64>,
    stored_state: Option<String>,
    continued: bool,
}

type Key = (i64, String);

// Python commits the identity change before asking the host to launch. The
// outcome describes that launch, including failure, not whether this edge exists.
// Walk backwards from each terminal attempt to map its predecessors to that tip.
const CHANGES: &str = "WITH RECURSIVE receipts AS MATERIALIZED (
    SELECT id,dispatch,type,body FROM message WHERE json_valid(body)),
    known_rounds(dispatch,attempt,n) AS (
        SELECT dispatch,attempt,round FROM dispatch_round
        UNION SELECT dispatch,body->>'$.attempt',body->>'$.round' FROM receipts
        UNION SELECT dispatch,body->>'$.toAttempt',body->>'$.round' FROM receipts
        UNION SELECT dispatch,coalesce(to_attempt,attempt),round FROM lifecycle_event
        UNION SELECT dispatch,attempt,round FROM watchdog_fault),
    moves AS (
        SELECT l.dispatch,l.id,coalesce(l.round,(SELECT min(k.n) FROM known_rounds k
            WHERE k.dispatch=l.dispatch AND k.attempt=l.to_attempt HAVING count(DISTINCT k.n)=1)) round,
            l.from_attempt old,l.to_attempt new,l.attempt FROM lifecycle_event l
        WHERE operation IN ('resume','handoff') AND from_attempt<>to_attempt),
    tips(dispatch,old,tip) AS (
        SELECT m.dispatch,m.old,m.new FROM moves m WHERE NOT EXISTS
            (SELECT 1 FROM moves n WHERE n.dispatch=m.dispatch AND n.old=m.new)
        UNION SELECT m.dispatch,m.old,t.tip FROM moves m JOIN tips t
            ON t.dispatch=m.dispatch AND t.old=m.new),
    invalid(dispatch) AS (
        SELECT dispatch FROM moves GROUP BY dispatch,old HAVING count(*)>1
        UNION SELECT dispatch FROM moves GROUP BY dispatch,new HAVING count(*)>1
        UNION SELECT m.dispatch FROM moves m JOIN moves next ON next.dispatch=m.dispatch AND next.old=m.new
            WHERE next.id<=m.id OR next.round NOT BETWEEN m.round AND m.round+1
        UNION SELECT m.dispatch FROM moves m WHERE m.attempt<>m.new OR m.round IS NULL
            OR NOT EXISTS(SELECT 1 FROM tips t WHERE t.dispatch=m.dispatch AND t.old=m.old)
            OR EXISTS(SELECT 1 FROM known_rounds k WHERE k.dispatch=m.dispatch AND
                ((k.attempt=m.old AND m.round NOT BETWEEN k.n AND k.n+1)
                OR (k.attempt=m.new AND k.n<>m.round))))";

pub(super) fn agree<T: PartialEq>(
    known: &mut Option<T>,
    other: Option<T>,
    field: &str,
) -> Result<()> {
    if let Some(value) = other {
        ensure!(
            known.as_ref().is_none_or(|old| old == &value),
            "conflicting {field}"
        );
        *known = Some(value);
    }
    Ok(())
}

pub(super) fn text(body: &Value, key: &str) -> Result<Option<String>> {
    match body.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::Null) | None => Ok(None),
        _ => bail!("invalid {key} in receipt"),
    }
}

fn matches(key: &Key, fact: &Evidence) -> bool {
    fact.round.is_none_or(|n| n == key.0) && fact.attempt.as_ref().is_none_or(|a| a == &key.1)
}

fn read_receipts(
    db: &Connection,
    id: &str,
    facts: &mut Vec<Evidence>,
    scope: &mut Option<String>,
    role: &mut Option<String>,
) -> Result<bool> {
    let mut modern = false;
    let mut messages =
        db.prepare("SELECT type,body,created,recipient FROM message WHERE dispatch=? ORDER BY id")?;
    let mut rows = messages.query([id])?;
    while let Some(row) = rows.next()? {
        let kind: Option<String> = row.get(0)?;
        let raw: String = row.get(1)?;
        let body: Value = serde_json::from_str(&raw).context("unreadable receipt body")?;
        ensure!(body.is_object(), "unreadable receipt object");
        let has_identity = [
            "round",
            "attempt",
            "fromAttempt",
            "toAttempt",
            "role",
            "scope",
        ]
        .iter()
        .any(|k| body.get(k).is_some());
        modern |= has_identity;
        let done = kind.as_deref() == Some("done");
        if !done && !has_identity {
            continue;
        }
        if done {
            let recipient: Option<String> = row.get(3)?;
            ensure!(
                recipient.as_deref().is_none_or(|s| s == "controller"),
                "unexpected done recipient"
            );
        }
        let round = body
            .get("round")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_i64()
                    .filter(|n| *n > 0)
                    .context("invalid round in receipt")
            })
            .transpose()?;
        let when: Option<f64> = row.get(2)?;
        let mut attempt = text(&body, "attempt")?;
        agree(
            &mut attempt,
            text(&body, "toAttempt")?,
            "receipt target attempt",
        )?;
        agree(scope, text(&body, "scope")?, "scope")?;
        agree(role, text(&body, "role")?, "role")?;
        if let Some(previous) = text(&body, "fromAttempt")? {
            facts.push(Evidence {
                attempt: Some(previous),
                ..Default::default()
            });
        }
        facts.push(Evidence {
            round,
            attempt,
            report: text(&body, "report")?,
            task: text(&body, "task")?,
            done: done.then_some((raw, when)),
            continued: kind.as_deref() == Some("continue"),
            started: if kind.as_deref() == Some("continue") {
                when
            } else {
                None
            },
            ..Default::default()
        });
    }

    Ok(modern)
}

fn read_audits(db: &Connection, id: &str, facts: &mut Vec<Evidence>) -> Result<bool> {
    let mut any = false;
    // from_attempt may name the previous round. Only the operation's target
    // is bound to its recorded round. Watchdog rows also pin a specific writer.
    let mut audits = db.prepare(
        "SELECT round,attempt,to_attempt,from_attempt FROM lifecycle_event WHERE dispatch=?1
        UNION ALL SELECT round,attempt,NULL,NULL FROM watchdog_fault WHERE dispatch=?1",
    )?;
    let mut rows = audits.query([id])?;
    while let Some(row) = rows.next()? {
        any = true;
        let previous: Option<String> = row.get(3)?;
        let target: Option<String> = row.get(2)?;
        let mut attempt = row.get(1)?;
        agree(&mut attempt, target, "audit target attempt")?;
        facts.push(Evidence {
            round: row.get(0)?,
            attempt,
            ..Default::default()
        });
        if let Some(previous) = previous {
            facts.push(Evidence {
                attempt: Some(previous),
                ..Default::default()
            });
        }
    }

    Ok(any)
}

fn bind_evidence(parent: &Evidence, mut facts: Vec<Evidence>) -> Result<BTreeMap<Key, Evidence>> {
    let mut history: BTreeMap<Key, Evidence> = BTreeMap::new();
    let mut reports: BTreeMap<&str, (BTreeSet<i64>, BTreeSet<&str>)> = BTreeMap::new();
    for f in facts.iter().chain([parent]) {
        ensure!(f.round.is_none_or(|n| n > 0), "invalid recorded round");
        ensure!(
            f.attempt.as_ref().is_none_or(|a| !a.is_empty()),
            "empty recorded attempt"
        );
        if let (Some(n), Some(a)) = (f.round, &f.attempt) {
            let entry = history.entry((n, a.clone())).or_default();
            agree(&mut entry.report, f.report.clone(), "attempt report")?;
        }
        if let Some(report) = f.report.as_deref() {
            let (rounds, attempts) = reports.entry(report).or_default();
            rounds.extend(f.round);
            attempts.extend(f.attempt.as_deref());
        }
    }
    // Partial identity fields can be joined on a shared, unambiguous report.
    for (report, (rounds, attempts)) in reports {
        if rounds.len() == 1 && attempts.len() == 1 {
            let key = (
                *rounds.first().unwrap(),
                attempts.first().unwrap().to_string(),
            );
            agree(
                &mut history.entry(key).or_default().report,
                Some(report.to_owned()),
                "attempt report",
            )?;
        }
    }
    ensure!(
        !history.is_empty(),
        "insufficient evidence for a round and attempt"
    );
    let distinct: BTreeSet<_> = history.keys().map(|(_, a)| a).collect();
    ensure!(
        distinct.len() == history.len(),
        "one attempt belongs to conflicting rounds"
    );

    // Bind complete identities first so report paths can disambiguate legacy receipts.
    facts.sort_by_key(|f| f.round.is_none() || f.attempt.is_none());
    for f in facts {
        let mut candidates = history.iter_mut().filter(|(key, known)| {
            matches(key, &f)
                && (known.report.is_none() || f.report.is_none() || known.report == f.report)
                && (known.task.is_none() || f.task.is_none() || known.task == f.task)
        });
        let first = candidates.next();
        if first.is_none()
            && f.done.is_some()
            && f.round.is_none()
            && f.attempt.is_none()
            && f.report.is_some()
        {
            // A pre-attempt dispatch can retain earlier reports solely as messages.
            continue;
        }
        let (_, known) = first.context("receipt or audit has no matching attempt")?;
        ensure!(
            candidates.next().is_none(),
            "ambiguous receipt or audit attempt"
        );
        if f.done.is_some() && f.attempt.is_none() && f.round.is_none() {
            ensure!(
                known.report.is_some() && known.report == f.report,
                "unbound pre-attempt completion"
            );
        }
        agree(&mut known.report, f.report, "attempt report")?;
        agree(&mut known.task, f.task, "attempt task")?;
        agree(
            &mut known.snapshot_row,
            f.snapshot_row,
            "snapshot rows for one identity",
        )?;
        agree(&mut known.stored_state, f.stored_state, "snapshot state")?;
        known.continued |= f.continued;
        agree(&mut known.started, f.started, "attempt start")?;
        // Completion reconciliation compares the ordered receipts against the
        // final snapshot. Identity recovery only needs evidence that a done exists.
        if known.done.is_none() {
            known.done = f.done;
        }
    }

    Ok(history)
}

fn recover(db: &Connection, id: &str) -> Result<()> {
    let (parent, mut scope, mut role, created) = db.query_row(
        "SELECT nullif(current_round,0),nullif(current_attempt,''),nullif(report,''),nullif(task,''),
                nullif(scope,''),nullif(role,''),created FROM dispatch WHERE id=?", [id],
        |r| Ok((Evidence { round:r.get(0)?, attempt:r.get(1)?, report:r.get(2)?, task:r.get(3)?, ..Default::default() },
                 r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?, r.get::<_,Option<f64>>(6)?)))?;

    let mut facts: Vec<Evidence> = db
        .prepare("SELECT round,attempt,coalesce(nullif(report,''),CASE WHEN json_valid(result) THEN result->>'$.report' END),
            nullif(task,''),state,rowid FROM dispatch_round WHERE dispatch=?")?
        .query_map([id], |r| {
            Ok(Evidence {
                round: r.get(0)?,
                attempt: r.get(1)?,
                report: r.get(2)?,
                task: r.get(3)?,
                stored_state: r.get(4)?,
                snapshot_row: r.get(5)?,
                ..Default::default()
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut modern =
        !facts.is_empty() || parent.attempt.is_some() || parent.round.is_some_and(|n| n != 1);
    modern |= read_receipts(db, id, &mut facts, &mut scope, &mut role)?;
    modern |= read_audits(db, id, &mut facts)?;
    if !modern {
        // An old completion with a single known report still identifies it when
        // the dispatch predates the report column.
        let mut by_report = BTreeMap::new();
        for f in facts {
            let report = f.report.context("legacy done has no report")?;
            if let Some((raw, _)) = f.done {
                let first = by_report.entry(report).or_insert_with(|| raw.clone());
                if first != &raw {
                    ensure!(
                        crate::completion::legacy_result(&serde_json::from_str(first)?)
                            && crate::completion::legacy_result(&serde_json::from_str(&raw)?),
                        "conflicting legacy done results"
                    );
                }
            }
        }
        let report = match parent.report {
            Some(path) => Some(path),
            None => {
                ensure!(by_report.len() <= 1, "ambiguous current legacy report");
                by_report.into_keys().next()
            }
        };
        db.execute(
            "UPDATE dispatch SET report=? WHERE id=? AND coalesce(report,'')='' AND ?1 IS NOT NULL",
            params![report, id],
        )?;
        return Ok(());
    }

    let invalid: bool = db.query_row(
        &format!("{CHANGES} SELECT EXISTS(SELECT 1 FROM invalid WHERE dispatch=?)"),
        [id],
        |r| r.get(0),
    )?;
    ensure!(!invalid, "conflicting attempt transition history");
    let mut history = bind_evidence(&parent, facts)?;
    let (&(last, _), _) = history.last_key_value().context("no historical attempts")?;
    let mut q = db.prepare(&format!(
        "{CHANGES} SELECT m.old,t.tip,m.round FROM moves m JOIN tips t
        ON t.dispatch=m.dispatch AND t.old=m.old WHERE m.dispatch=?"
    ))?;
    let mut successors: BTreeMap<String, String> = BTreeMap::new();
    let mut last_tip = None;
    let mut rows = q.query([id])?;
    while let Some(row) = rows.next()? {
        let tip: String = row.get(1)?;
        if row.get::<_, i64>(2)? == last {
            agree(
                &mut last_tip,
                Some(tip.clone()),
                "transition tips in the latest round",
            )?;
        }
        successors.insert(row.get(0)?, tip);
    }
    // A complete pointer to a real older snapshot can lag a committed continue.
    // Move it only if both ends have independent snapshot evidence. An invented
    // first attempt or a half-recorded transition cannot authorize a new writer.
    let old_key = parent.round.zip(parent.attempt.clone());
    let stale = old_key.as_ref().is_some_and(|key| {
        (key.0 < last || successors.contains_key(&key.1))
            && history.get(key).is_some_and(|f| {
                f.snapshot_row.is_some()
                    && parent
                        .task
                        .as_ref()
                        .is_none_or(|t| f.task.as_ref() == Some(t))
                    && parent
                        .report
                        .as_ref()
                        .is_none_or(|r| f.report.as_ref() == Some(r))
            })
    });
    let mut candidates = history.iter().filter(|(key, f)| {
        key.0 == last
            && !successors.contains_key(&key.1)
            && if stale {
                f.snapshot_row.is_some()
            } else {
                matches(key, &parent)
                    && (parent.report.is_none() || f.report.is_none() || f.report == parent.report)
                    && (parent.task.is_none() || f.task.is_none() || f.task == parent.task)
            }
    });
    let (current, _) = candidates
        .next()
        .context("current dispatch conflicts with historical attempts or report")?;
    ensure!(candidates.next().is_none(), "ambiguous current attempt");
    let current = current.clone();
    ensure!(
        !history.iter().any(|(key, f)| f.continued
            && key.0 == last
            && *key != current
            && successors.get(&key.1) != Some(&current.1)),
        "continue conflicts with current attempt"
    );
    // A second disconnected lineage cannot authorize a writer in this round.
    ensure!(
        last_tip.is_none_or(|tip| tip == current.1),
        "current attempt conflicts with transition history"
    );
    let selected = history.get_mut(&current).unwrap();
    if !stale {
        agree(&mut selected.report, parent.report, "current report")?;
        agree(&mut selected.task, parent.task, "current task")?;
    }
    ensure!(
        selected.report.is_some(),
        "insufficient evidence for current report"
    );
    if scope.is_none() {
        let managed: bool =
            db.query_row("SELECT EXISTS(SELECT 1 FROM controller_owner)", [], |r| {
                r.get(0)
            })?;
        ensure!(
            !managed,
            "cannot establish owner scope for historical attempts"
        );
    }
    db.execute("UPDATE dispatch SET current_round=?,current_attempt=?,report=?,task=?,scope=?,role=? WHERE id=?",
        params![current.0,current.1,selected.report,selected.task,scope,role,id])?;

    save_snapshots(db, id, history, created)
}

fn save_snapshots(
    db: &Connection,
    id: &str,
    history: BTreeMap<Key, Evidence>,
    created: Option<f64>,
) -> Result<()> {
    let single_first_attempt = history.keys().filter(|(round, _)| *round == 1).count() == 1;
    for ((round, attempt), f) in history {
        if let Some(row_id) = f.snapshot_row {
            let state = f.stored_state.as_deref().unwrap_or("");
            ensure!(
                !state.is_empty() || f.done.is_some(),
                "missing snapshot state for round {round}, attempt {attempt}"
            );
            // Keep the original row and its callback fields when completing its identity.
            db.execute(
                "UPDATE dispatch_round SET task=coalesce(nullif(task,''),?1),report=coalesce(nullif(report,''),?2),
                    round=?3, attempt=?4 WHERE rowid=?5",
                params![f.task, f.report, round, attempt, row_id],
            )?;
            continue;
        }
        ensure!(
            f.done.is_some(),
            "insufficient outcome evidence for round {round}, attempt {attempt}"
        );
        ensure!(
            f.report.is_some(),
            "missing report for round {round}, attempt {attempt}"
        );
        let started = f.started.or(if round == 1 && single_first_attempt {
            created
        } else {
            None
        });
        db.execute(
            "INSERT INTO dispatch_round(dispatch,round,attempt,task,report,state,created)
            VALUES(?,?,?,?,?,'completed',?)",
            params![id, round, attempt, f.task, f.report, started],
        )?;
    }
    Ok(())
}

// Include a ledger already opened by an older migrator that created a new
// first attempt despite a receipt naming a different, now missing snapshot.
pub fn candidates(db: &Connection) -> Result<Vec<String>> {
    let mut query = db.prepare(&format!("{CHANGES}, continues AS (
            SELECT id,dispatch,body->>'$.round' n,body->>'$.attempt' a,
                body->>'$.task' task,body->>'$.report' report FROM receipts WHERE type='continue'),
        possible AS (
            SELECT m.*,r.round,r.attempt,r.task recorded_task,r.report recorded_report,
                ((m.task IS NULL OR r.task IS NULL OR m.task=r.task)
                  AND (m.report IS NULL OR r.report IS NULL OR m.report=r.report)) compatible
            FROM continues m JOIN dispatch_round r ON r.dispatch=m.dispatch
              AND (m.n IS NULL OR m.n=r.round) AND (m.a IS NULL OR m.a=r.attempt)
            WHERE m.n IS NOT NULL OR m.a IS NOT NULL),
        bindings AS (
            SELECT *, count(*) OVER(PARTITION BY id) choices,
                sum(compatible) OVER(PARTITION BY id) compatible_count FROM possible),
        attested(id,n,a) AS (
            SELECT dispatch,body->>'$.round',body->>'$.attempt' FROM receipts
            UNION ALL SELECT dispatch,body->>'$.round',body->>'$.toAttempt' FROM receipts
                WHERE body->>'$.toAttempt' IS NOT NULL
            UNION ALL SELECT dispatch,NULL,body->>'$.fromAttempt' FROM receipts
                WHERE body->>'$.fromAttempt' IS NOT NULL
            UNION ALL SELECT dispatch,round,attempt FROM lifecycle_event
            UNION ALL SELECT dispatch,round,to_attempt FROM lifecycle_event WHERE to_attempt IS NOT NULL
            UNION ALL SELECT dispatch,NULL,from_attempt FROM lifecycle_event WHERE from_attempt IS NOT NULL
            UNION ALL SELECT dispatch,round,attempt FROM watchdog_fault)
        SELECT id FROM dispatch d WHERE nullif(current_attempt,'') IS NULL OR coalesce(current_round,0)=0
          OR coalesce(scope,'')='' OR coalesce(role,'')=''
          OR NOT EXISTS(SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id)
          OR EXISTS(SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id AND r.round=d.current_round
              AND r.attempt=d.current_attempt AND ((r.report IS NOT NULL AND nullif(d.report,'') IS NULL)
              OR (nullif(r.task,'') IS NOT nullif(d.task,''))))
          OR EXISTS(SELECT 1 FROM dispatch_round r WHERE r.dispatch=d.id
              AND (r.round IS NULL OR r.attempt IS NULL OR coalesce(r.state,'')='' OR r.round>d.current_round))
        UNION SELECT d.id FROM dispatch d JOIN invalid i ON i.dispatch=d.id
        UNION SELECT d.id FROM dispatch d JOIN moves m ON m.dispatch=d.id
            JOIN tips t ON t.dispatch=m.dispatch AND t.old=m.old
            WHERE m.old=d.current_attempt OR (m.round=d.current_round AND t.tip<>d.current_attempt)
        UNION SELECT d.id FROM bindings b JOIN dispatch d ON d.id=b.dispatch
          WHERE (b.choices>1 AND b.compatible_count<>1)
            OR ((b.choices=1 OR b.compatible) AND (
               (b.task IS NOT NULL AND b.recorded_task IS NOT b.task)
               OR (b.report IS NOT NULL AND b.recorded_report IS NOT b.report)
               OR (b.round=d.current_round AND b.attempt=d.current_attempt AND (
                   (b.task IS NOT NULL AND d.task IS NOT b.task)
                   OR (b.report IS NOT NULL AND d.report IS NOT b.report)))
               OR (b.round=d.current_round AND b.attempt<>d.current_attempt AND NOT EXISTS(
                   SELECT 1 FROM tips t WHERE t.dispatch=d.id AND t.old=b.attempt AND t.tip=d.current_attempt))))
        UNION SELECT d.id FROM message m JOIN dispatch d ON d.id=m.dispatch
            WHERE m.type='continue' AND NOT json_valid(m.body)
        UNION SELECT e.id FROM attested e JOIN dispatch d ON d.id=e.id
          WHERE (e.n IS NOT NULL OR e.a IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM dispatch_round r
              WHERE r.dispatch=e.id AND (e.n IS NULL OR r.round=e.n) AND (e.a IS NULL OR r.attempt=e.a))"))?;
    let rows = query.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn recover_missing(db: &Connection, ids: Vec<String>) -> Result<()> {
    for id in ids {
        recover(db, &id).with_context(|| format!("cannot recover dispatch {id}"))?;
    }
    Ok(())
}
