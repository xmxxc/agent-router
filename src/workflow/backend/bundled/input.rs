//! Ordinary Codex input on the current PTY and input sequence.
use super::{close_blocks_callback, registered_ticket};
use crate::{bundled::InputSession, workflow::controller::ControllerError};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, thread::sleep, time::Duration};
use wfc_herdr::{classify_reply_capture, matches_single_line_draft, reply_input_paused, ReplyBox};

const CODEX_PASTE_SETTLE: Duration = Duration::from_millis(500);
pub(in crate::workflow) const APPENDED_USER_ENTRY: &str = "bundled-appended-user-entry";

struct Composer {
    visible: String,
    composer: ReplyBox,
    sequence: u64,
}

#[derive(Clone, Copy)]
enum Submission {
    Reply,
    Nudge { earlier_reminders: usize },
    Continue,
}

fn observe(session: &InputSession) -> Result<Composer> {
    let answer = session.read()?;
    let capture = answer["data"]["styled"]
        .as_str()
        .context("bundled runtime did not return viewport styles")?;
    let (visible, composer) = classify_reply_capture("codex", capture)
        .context("could not classify the current Codex viewport")?;
    let sequence = answer["last_input"]
        .as_u64()
        .context("missing bundled input sequence")?;
    Ok(Composer {
        visible,
        composer,
        sequence,
    })
}

fn rows(viewport: &str) -> Vec<&str> {
    viewport
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .collect()
}

fn prompt_row(rows: &[&str]) -> Option<usize> {
    rows.iter().rposition(|row| {
        row.strip_prefix(['›', '»'])
            .is_some_and(|body| body.is_empty() || body.starts_with(' '))
    })
}

fn history(viewport: &str) -> Option<Vec<&str>> {
    let mut rows = rows(viewport);
    let composer = prompt_row(&rows)?;
    rows.truncate(composer);
    let first = rows.iter().position(|row| !row.is_empty())?;
    rows.drain(..first);
    Some(rows)
}

// An ordinary Codex wrap may split a word or omit one space at a row boundary.
// Match every byte of the submitted single-line brief, including its callback reference.
fn complete_brief(rows: &[&str], text: &str) -> Option<()> {
    let end = rows.iter().position(|row| row.is_empty())?;
    let (first, rest) = rows[..end].split_first()?;
    let first = first
        .strip_prefix(['›', '»'])?
        .strip_prefix(' ')
        .filter(|body| !body.is_empty())?;
    let mut remaining = text.strip_prefix(first)?;
    for row in rest {
        let body = row.strip_prefix("  ").filter(|body| !body.is_empty())?;
        remaining = remaining
            .strip_prefix(body)
            .or_else(|| remaining.strip_prefix(' ')?.strip_prefix(body))?;
    }
    remaining.is_empty().then_some(())
}

fn appended_brief(before: &[&str], after: &[&str], text: &str) -> Option<()> {
    let added = after.strip_prefix(before)?;
    let first = added.iter().position(|row| !row.is_empty())?;
    complete_brief(&added[first..], text)?;
    (!added[first + 1..]
        .iter()
        .enumerate()
        .any(|(i, _)| complete_brief(&added[first + 1 + i..], text).is_some()))
    .then_some(())
}

fn appended_reminder(before: &[&str], after: &[&str], text: &str) -> bool {
    let entry = |row: &str| row.strip_prefix("› ") == Some(text);
    after.strip_prefix(before).is_some_and(|added| {
        let Some(first) = added.iter().position(|row| !row.is_empty()) else {
            return false;
        };
        // An exact first row may be the start of a longer, wrapped entry.
        complete_reminder(&added[first..], text)
            && added.iter().filter(|row| entry(row)).count() == 1
    })
}

fn complete_reminder(rows: &[&str], text: &str) -> bool {
    matches!(rows, [row, "", ..] if row.strip_prefix("› ") == Some(text))
}

pub(in crate::workflow) fn submit_reply(
    state: &Path,
    row: &Value,
    text: &str,
    begin: &mut impl FnMut(bool) -> Result<()>,
    check_write: impl Fn() -> Result<()>,
) -> Result<Value> {
    submit(
        state,
        row,
        text,
        Submission::Reply,
        begin,
        check_write,
        || {},
    )
}

pub(in crate::workflow) fn submit_nudge(
    state: &Path,
    row: &Value,
    text: &str,
    earlier_reminders: usize,
    prepare: impl FnMut() -> Result<()>,
    check_write: impl Fn() -> Result<()>,
) -> (Result<Value>, bool) {
    submit_worker_input(
        state,
        row,
        text,
        Submission::Nudge { earlier_reminders },
        prepare,
        check_write,
    )
}

pub(in crate::workflow) fn submit_continue(
    state: &Path,
    row: &Value,
    text: &str,
    prepare: impl FnMut() -> Result<()>,
    check_write: impl Fn() -> Result<()>,
) -> (Result<Value>, bool) {
    submit_worker_input(state, row, text, Submission::Continue, prepare, check_write)
}

fn submit_worker_input(
    state: &Path,
    row: &Value,
    text: &str,
    submission: Submission,
    mut prepare: impl FnMut() -> Result<()>,
    check_write: impl Fn() -> Result<()>,
) -> (Result<Value>, bool) {
    let mut prepared = false;
    let mut first_frame_may_have_written = false;
    let answer = submit(
        state,
        row,
        text,
        submission,
        &mut |_| {
            prepare()?;
            prepared = true;
            Ok(())
        },
        check_write,
        || first_frame_may_have_written = true,
    );
    let answer = match answer {
        Err(error) if !prepared && !error.is::<ControllerError>() => {
            Ok(json!({"result":"definitely-not-sent","diagnostic":format!("{error:#}")}))
        }
        answer => answer,
    };
    (answer, first_frame_may_have_written)
}

// The caller owns scope and execution locks. Begin revalidates the original
// ledger claim, execution and current owner before each frame.
fn submit(
    state: &Path,
    row: &Value,
    text: &str,
    submission: Submission,
    begin: &mut impl FnMut(bool) -> Result<()>,
    check_write: impl Fn() -> Result<()>,
    mark_possible_write: impl FnOnce(),
) -> Result<Value> {
    let operation = match submission {
        Submission::Reply => "reply",
        Submission::Nudge { .. } => "nudge",
        Submission::Continue => "continue",
    };
    ensure!(
        row["host"] == "codex",
        "bundled {operation} currently requires Codex"
    );
    ensure!(
        !text.is_empty() && !text.chars().any(char::is_control),
        "invalid bundled {operation} text"
    );
    let create = &row["create"];
    let ticket = registered_ticket(row)?;
    ensure!(
        !close_blocks_callback(state, row)?,
        "bundled {operation} requires the registered current writable execution"
    );
    let session = InputSession::open(state, ticket, &create["identity"])?;
    let before = observe(&session)?;
    ensure!(
        matches!(before.composer, ReplyBox::Empty)
            && !reply_input_paused(&before.visible, &before.visible),
        "Codex does not show a verified empty writable composer"
    );
    let baseline = if matches!(submission, Submission::Reply) {
        None
    } else {
        Some(history(&before.visible).context("Codex has no stable visible transcript")?)
    };
    if let (Submission::Nudge { earlier_reminders }, Some(rows)) = (submission, &baseline) {
        ensure!(
            rows.windows(2)
                .filter(|rows| complete_reminder(rows, text))
                .count()
                >= earlier_reminders,
            "a prior matching reminder is outside the bundled viewport"
        );
    }

    begin(true)?;
    let ready = match observe(&session) {
        Ok(ready)
            if matches!(ready.composer, ReplyBox::Empty)
                && ready.sequence == before.sequence
                && baseline
                    .as_ref()
                    .is_none_or(|rows| history(&ready.visible).as_ref() == Some(rows))
                && !reply_input_paused(&before.visible, &ready.visible) =>
        {
            ready
        }
        _ => {
            return Ok(json!({"result":"definitely-not-sent",
            "diagnostic":"Codex composer changed before text was sent"}))
        }
    };
    let paste = format!("\x1b[200~{text}\x1b[201~");
    let (pasted, may_have_written) = session.input(ready.sequence, &paste, &check_write);
    if may_have_written {
        mark_possible_write();
    }
    let pasted = match pasted {
        Ok(pasted) => pasted,
        Err(error) if error.is::<ControllerError>() => return Err(error),
        Err(error) => {
            return Ok(json!({"result":"definitely-not-sent","diagnostic":format!("{error:#}")}));
        }
    };
    if pasted["result"] != "accepted" {
        return Ok(pasted);
    }

    // The ledger already has a durable claim. Any error from here leaves it
    // unresolved. Codex needs Enter outside the paste burst.
    sleep(CODEX_PASTE_SETTLE);
    begin(false)
        .map_err(|error| anyhow::anyhow!("{operation} draft could not be authorized: {error:#}"))?;
    let draft = observe(&session)?;
    let exact_draft = match (&draft.composer, submission) {
        (ReplyBox::Draft(Some(shown)), _) => matches_single_line_draft(shown, text),
        (ReplyBox::Draft(None), Submission::Continue) => {
            let rows = rows(&draft.visible);
            prompt_row(&rows).is_some_and(|start| complete_brief(&rows[start..], text).is_some())
        }
        _ => false,
    };
    ensure!(
        draft.sequence
            == pasted["last_input"]
                .as_u64()
                .context("missing paste acknowledgement")?
            && !reply_input_paused(&before.visible, &draft.visible)
            && baseline
                .as_ref()
                .is_none_or(|rows| history(&draft.visible).as_ref() == Some(rows))
            && exact_draft,
        "Codex did not show the complete {operation} draft before Enter"
    );
    let entered = session
        .input(draft.sequence, "\r", &check_write)
        .0
        .map_err(|error| anyhow::anyhow!("{operation} Enter could not be authorized: {error:#}"))?;
    ensure!(
        entered["result"] == "accepted",
        "bundled Enter was not confirmed"
    );
    for _ in 0..2 {
        sleep(Duration::from_millis(300));
        let after = observe(&session)?;
        ensure!(
            Some(after.sequence) == entered["last_input"].as_u64()
                && !reply_input_paused(&before.visible, &after.visible)
                && matches!(after.composer, ReplyBox::Empty),
            "Codex did not confirm submission in the bundled viewport"
        );
        if let Some(rows) = &baseline {
            ensure!(
                history(&after.visible).is_some_and(|after| match submission {
                    Submission::Continue => appended_brief(rows, &after, text).is_some(),
                    _ => appended_reminder(rows, &after, text),
                }),
                "Codex did not show a new {operation} entry in the bundled viewport"
            );
        }
    }
    if baseline.is_some() {
        return Ok(json!({"result":"accepted","inputEvidence":APPENDED_USER_ENTRY}));
    }
    Ok(json!({"result":"accepted"}))
}

#[cfg(test)]
mod tests {
    use super::{appended_brief, appended_reminder, complete_brief};

    #[test]
    fn bundled_continue_requires_the_complete_wrapped_brief() {
        let original = ["› Prior completed task", ""];
        let text = "[router] Next task --attempt a-123 --callback-ref abcd";
        let draft = [
            "› [router] Next task",
            "  --attempt a-123 --callback-ref ab",
            "  cd",
            "",
        ];
        assert!(complete_brief(&draft, text).is_some());
        assert!(complete_brief(&draft[..3], text).is_none());
        assert!(complete_brief(
            &draft,
            "[router] Next task --attempt a-123 --callback-ref abce"
        )
        .is_none());
        let with_entry = [original.as_slice(), draft.as_slice()].concat();
        assert!(appended_brief(&original, &with_entry, text).is_some());
        assert!(appended_brief(&original, &draft, text).is_none());
    }

    #[test]
    fn bundled_reminder_requires_a_complete_new_user_entry() {
        let text = "[router] Message from the controller: ordinary reminder";
        let line = "› [router] Message from the controller: ordinary reminder";
        let previous = ["› Initial task", ""];
        assert!(appended_reminder(
            &previous,
            &[previous[0], "", line, ""],
            text
        ));
        assert!(!appended_reminder(
            &previous,
            &[previous[0], "", line, "  wrapped continuation", ""],
            text
        ));
        assert!(!appended_reminder(
            &previous,
            &[previous[0], "", line, "", line, ""],
            text
        ));
        assert!(!appended_reminder(&previous, &[line, ""], text));
    }
}
