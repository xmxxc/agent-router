//! Use the current PTY viewport and input sequence for an ordinary Codex reply.
use super::close_blocks_callback;
use crate::{bundled::InputSession, workflow::controller::ControllerError};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, thread::sleep, time::Duration};
use wfc_herdr::{classify_reply_capture, matches_single_line_draft, reply_input_paused, ReplyBox};

const CODEX_PASTE_SETTLE: Duration = Duration::from_millis(500);

struct Composer {
    visible: String,
    composer: ReplyBox,
    sequence: u64,
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

// The caller owns scope and execution locks, and begin revalidates the
// question, ledger claim, execution and current owner before each frame.
pub(in crate::workflow) fn submit_reply(
    state: &Path,
    row: &Value,
    text: &str,
    begin: &mut impl FnMut(bool) -> Result<()>,
    check_write: impl Fn() -> Result<()>,
) -> Result<Value> {
    ensure!(
        row["host"] == "codex",
        "bundled reply currently requires Codex"
    );
    ensure!(
        !text.is_empty() && !text.chars().any(char::is_control),
        "invalid bundled reply text"
    );
    let create = &row["create"];
    let ticket = &create["ticket"];
    ensure!(
        row["createPending"] != true
            && create["result"] == "accepted"
            && ticket.is_object()
            && ticket["unit"] == row["dispatch"]
            && ticket["attempt"] == row["attempt"]
            && row["backendHandle"] == *ticket
            && !close_blocks_callback(state, row)?,
        "bundled reply requires the registered current writable execution"
    );
    let session = InputSession::open(state, ticket, &create["identity"])?;
    let before = observe(&session)?;
    ensure!(
        matches!(before.composer, ReplyBox::Empty)
            && !reply_input_paused(&before.visible, &before.visible),
        "Codex does not show a verified empty writable composer"
    );

    begin(true)?;
    let ready = match observe(&session) {
        Ok(ready)
            if matches!(ready.composer, ReplyBox::Empty)
                && ready.sequence == before.sequence
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
    let pasted = match session.input(ready.sequence, &paste, &check_write) {
        Ok(pasted) => pasted,
        Err(error) if error.is::<ControllerError>() => return Err(error),
        Err(error) => {
            return Ok(json!({"result":"definitely-not-sent","diagnostic":format!("{error:#}")}));
        }
    };
    if pasted["result"] != "accepted" {
        return Ok(pasted);
    }

    // The existing ledger is already marked uncertain. Any error from here
    // leaves the question claimed. Codex needs Enter outside the paste burst.
    sleep(CODEX_PASTE_SETTLE);
    begin(false)
        .map_err(|error| anyhow::anyhow!("reply draft could not be authorized: {error:#}"))?;
    let draft = observe(&session)?;
    ensure!(
        draft.sequence
            == pasted["last_input"]
                .as_u64()
                .context("missing paste acknowledgement")?
            && !reply_input_paused(&before.visible, &draft.visible)
            && matches!(draft.composer, ReplyBox::Draft(Some(shown)) if matches_single_line_draft(&shown, text)),
        "Codex did not show the complete reply draft before Enter"
    );
    let entered = session
        .input(draft.sequence, "\r", &check_write)
        .map_err(|error| anyhow::anyhow!("reply Enter could not be authorized: {error:#}"))?;
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
    }
    Ok(json!({"result":"accepted"}))
}
