//! Receipt transitions and operator evidence. The caller holds the inbox lock
//! from lookup until the updated receipt and its evidence are published.
use super::{options, Inbox, Message};
use crate::workflow::state_dir;
use anyhow::{bail, ensure, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;

pub const ACK_USAGE: &str = "wfc ack MESSAGE [--body BODY]";
pub const RESOLVE_USAGE: &str =
    "wfc resolve MESSAGE --actor ACTOR [--source SOURCE] --reason REASON --evidence EVIDENCE --body-hash SHA256";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum State {
    Unsent,
    Unknown,
    Received,
    Processed,
}

#[derive(Serialize, Deserialize)]
#[serde(try_from = "StoredDelivery", rename_all = "camelCase")]
pub(super) struct Delivery {
    pub state: State,
    pub status_version: u64,
    // Only the boolean-only publisher predates both status versions and Git
    // snapshots. Versioned rows may also lack a hash after an inbox rewrite.
    #[serde(skip)]
    pub legacy_unclaimed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDelivery {
    state: Option<State>,
    status_version: Option<u64>,
    received: Option<bool>,
}

impl TryFrom<StoredDelivery> for Delivery {
    type Error = &'static str;

    fn try_from(stored: StoredDelivery) -> Result<Self, Self::Error> {
        let (state, version) = match (stored.state, stored.status_version, stored.received) {
            (Some(state), Some(version @ 1..), _) => (state, version),
            (None, None, Some(true)) => (State::Received, 2),
            (None, None, Some(false)) => (State::Unsent, 1),
            _ => return Err("invalid receipt state/version"),
        };
        Ok(Self {
            state,
            status_version: version,
            legacy_unclaimed: stored.state.is_none() && stored.received == Some(false),
        })
    }
}

impl Delivery {
    pub fn unsent() -> Self {
        Self {
            state: State::Unsent,
            status_version: 1,
            legacy_unclaimed: false,
        }
    }

    pub fn next_version(&self) -> Result<u64> {
        self.status_version
            .checked_add(1)
            .context("receipt version exhausted")
    }

    pub fn advance(&mut self, state: State) -> Result<()> {
        self.status_version = self.next_version()?;
        self.state = state;
        Ok(())
    }

    pub fn delivered(&self) -> bool {
        matches!(self.state, State::Received | State::Processed)
    }
}

pub(super) fn timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Claim {
    actor: String,
    source: String,
    reason: String,
    evidence: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Resolution {
    #[serde(flatten)]
    claim: Claim,
    at: String,
}

enum Confirmation<'a> {
    Ack { body: Option<&'a str> },
    Resolve { claim: Claim, body_hash: &'a str },
}

pub struct Confirm<'a> {
    id: &'a str,
    action: Confirmation<'a>,
}

impl<'a> Confirm<'a> {
    pub fn parse(command: &str, args: &'a [OsString]) -> Result<Self> {
        let mut id = None;
        let action = if command == "ack" {
            let [body] = options(args, ["--body"], Some((&mut id, "message id")))?;
            Confirmation::Ack { body }
        } else {
            let [actor, source, reason, evidence, hash] = options(
                args,
                [
                    "--actor",
                    "--source",
                    "--reason",
                    "--evidence",
                    "--body-hash",
                ],
                Some((&mut id, "message id")),
            )?;
            let required = |value: Option<&'a str>, name| -> Result<String> {
                Ok(value
                    .filter(|v| !v.trim().is_empty())
                    .context(name)?
                    .to_owned())
            };
            Confirmation::Resolve {
                claim: Claim {
                    actor: required(actor, "missing actor")?,
                    source: required(Some(source.unwrap_or("operator")), "missing source")?,
                    reason: required(reason, "missing reason")?,
                    evidence: required(evidence, "missing evidence")?,
                },
                body_hash: hash.context("missing --body-hash")?,
            }
        };
        Ok(Self {
            id: id.context("missing message id")?,
            action,
        })
    }

    pub fn run(self) -> Result<Value> {
        let state = state_dir()?;
        let mut inbox = Inbox::open(&state)?;
        let numeric = self.id.parse::<u64>().ok();
        let index = inbox
            .messages
            .iter()
            .position(|m| {
                m.reply_payload.is_none() && (Some(m.id) == numeric || m.message_id == self.id)
            })
            .with_context(|| format!("no controller message with id {}", self.id))?;
        let (earlier, tail) = inbox.messages.split_at_mut(index);
        let message = &mut tail[0];
        let duplicate = self.action.apply(message, earlier)?;
        let receipt = message.receipt(duplicate);
        if duplicate {
            // The previous rename can be visible even if syncing its directory
            // failed. Complete that durability step before acknowledging a retry.
            inbox
                .sync_directory()
                .context("sync controller confirmation")?;
        } else {
            inbox.save().context("save controller confirmation")?;
        }
        Ok(receipt)
    }
}

impl Confirmation<'_> {
    fn apply(self, message: &mut Message, earlier: &[Message]) -> Result<bool> {
        match self {
            Self::Ack { body } => {
                ensure!(message.delivery.delivered(), "ACK requires received");
                if let Some(body) = body {
                    ensure!(
                        message.matches_ack_body(body, earlier),
                        "ACK conflicts with its body"
                    );
                }
                if message.delivery.state == State::Processed {
                    return Ok(true);
                }
                message.delivery.advance(State::Processed)?;
            }
            Self::Resolve { claim, body_hash } => {
                let matches_body = message.matches_hash(body_hash);
                if message.delivery.delivered() {
                    if matches_body
                        && message
                            .resolution
                            .as_ref()
                            .is_some_and(|r| r.claim == claim)
                    {
                        return Ok(true);
                    }
                    bail!("message conflicts with its existing resolution");
                }
                ensure!(
                    message.delivery.state == State::Unknown,
                    "resolve requires unknown"
                );
                ensure!(matches_body, "body hash does not match the stored receipt");
                message.delivery.advance(State::Received)?;
                message.body_hash = Some(body_hash.to_owned());
                message.resolution = Some(Resolution {
                    claim,
                    at: timestamp(),
                });
            }
        }
        Ok(false)
    }
}
