use super::{controller, messages, Result, NO_SEND_RECOVERY};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const RESTORED_FIELDS: [&str; 7] = [
    "round",
    "attempt",
    "task",
    "report",
    "rounds",
    "executionId",
    controller::CALLBACK_REF_HASH,
];

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Claims {
    audit: String,
    assignment_hash: String,
    previous: Option<Map<String, Value>>,
}

fn commitment(proof: &str, claims: &Claims) -> Result<String> {
    Ok(messages::body_hash(&serde_json::to_string(&json!([
        proof, claims
    ]))?))
}

pub(super) fn stage(before: &Value, pending: &mut Value, proof: &str, index: usize) -> Result<()> {
    pending
        .as_object_mut()
        .context("invalid input assignment")?
        .remove(NO_SEND_RECOVERY);
    let event = &pending["audit"][index];
    let claims = Claims {
        audit: event["id"]
            .as_str()
            .context("missing input audit ID")?
            .to_owned(),
        assignment_hash: messages::body_hash(&serde_json::to_string(pending)?),
        previous: (event["operation"] == "continue").then(|| {
            RESTORED_FIELDS
                .iter()
                .filter_map(|&name| {
                    before
                        .get(name)
                        .map(|value| (name.to_owned(), value.clone()))
                })
                .collect()
        }),
    };
    pending[NO_SEND_RECOVERY] = json!({"commitment":commitment(proof, &claims)?,"claims":claims});
    Ok(())
}

pub(super) fn restore(mut row: Value, proof: &str) -> Result<(Value, Value)> {
    let record = row.as_object_mut().context("invalid input assignment")?;
    let evidence = record
        .remove(NO_SEND_RECOVERY)
        .context("no pending zero-send proof to verify")?;
    let claims: Claims = serde_json::from_value(evidence["claims"].clone())
        .context("invalid pending zero-send claims")?;
    ensure!(
        evidence["commitment"] == commitment(proof, &claims)?,
        "invalid zero-send proof"
    );
    ensure!(
        claims.assignment_hash == messages::body_hash(&serde_json::to_string(&row)?),
        "assignment changed since the input was staged; keep the input pending"
    );
    let event = row["audit"]
        .as_array()
        .and_then(|events| events.last())
        .cloned()
        .context("missing pending input audit")?;
    ensure!(
        event["id"] == claims.audit
            && event["result"] == "pending"
            && event["round"] == row["round"]
            && event["attempt"] == row["attempt"],
        "zero-send proof does not match the pending input audit"
    );
    match (event["operation"].as_str(), claims.previous) {
        (Some("continue"), Some(mut previous)) => {
            let record = row.as_object_mut().context("invalid input assignment")?;
            for name in RESTORED_FIELDS {
                if let Some(value) = previous.remove(name) {
                    record.insert(name.to_owned(), value);
                } else {
                    record.remove(name);
                }
            }
        }
        (Some("nudge"), None) => (),
        _ => anyhow::bail!("zero-send proof does not match the input operation"),
    }
    ensure!(
        row["round"] == event["fromRound"] && row["attempt"] == event["fromAttempt"],
        "zero-send proof does not match the original round"
    );
    Ok((row, event))
}

pub(super) fn settlement_error(
    error: anyhow::Error,
    proof: Option<&str>,
    dispatch: &str,
    audit: &Value,
) -> anyhow::Error {
    let Some(proof) = proof else { return error };
    controller::ControllerError(json!({"error":"bundled_no_send_recovery_required",
        "result":"definitely-not-sent", "dispatch":dispatch, "audit":audit,
        "noSendProof":proof, "diagnostic":format!("{error:#}") }))
    .into()
}

pub(super) fn proof_after_refusal<'a>(
    proof: Option<&'a str>,
    rejected_before_write: bool,
    outcome: &str,
) -> Option<&'a str> {
    proof.filter(|_| rejected_before_write && outcome == "definitely-not-sent")
}

pub(super) fn history_conflicts(event: &Value, history: &[Value]) -> Result<bool> {
    let key = messages::Inbox::worker_input_key(event)?;
    Ok(history.iter().any(|message| {
        message["messageId"] == key
            || (event["operation"] == "continue"
                && message["round"] == event["round"]
                && message["attempt"] == event["attempt"])
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_or_new_round_callback_conflicts_with_zero_send() -> Result<()> {
        let mut event =
            json!({"id":"i-fixture","operation":"continue","round":2,"attempt":"a-next"});
        let previous = json!({"messageId":"m-old","type":"done","round":1,"attempt":"a-original"});
        let callback = json!({"messageId":"m-callback","type":"done","round":2,"attempt":"a-next"});
        let receipt = json!({"messageId":"m-fixture"});
        assert!(!history_conflicts(&event, &[previous])?);
        assert!(history_conflicts(&event, &[callback.clone()])?);
        assert!(history_conflicts(&event, &[receipt.clone()])?);
        event["operation"] = json!("nudge");
        assert!(!history_conflicts(&event, &[callback])?);
        assert!(history_conflicts(&event, &[receipt])?);
        Ok(())
    }

    #[test]
    fn proof_recovers_only_the_staged_input() -> Result<()> {
        let proof = "0123456789abcdef";
        let before = json!({"dispatch":"d-12345678","round":2,"attempt":"a-before",
            "scope":"synthetic","report":"first.md","rounds":[{"result":{"messageId":"original"}}],
            "workerCallbackRefHash":"previous-reference"});
        for operation in ["continue", "nudge"] {
            let mut row = before.clone();
            if operation == "continue" {
                row["round"] = json!(3);
                row["attempt"] = json!("a-after");
                row["report"] = json!("next.md");
                row["rounds"] = json!([]);
                row["workerCallbackRefHash"] = json!("new-reference");
                row["executionId"] = json!("native-launch");
            }
            let event = json!({"id":"i-original", "operation":operation, "result":"pending",
                "fromRound":before["round"], "fromAttempt":before["attempt"],
                "round":row["round"], "attempt":row["attempt"]});
            row["audit"] = json!([event]);
            assert!(restore(row.clone(), proof).is_err());
            stage(&before, &mut row, proof, 0)?;
            assert!(restore(row.clone(), "different proof").is_err());
            for (key, value) in [
                ("round", json!(9)),
                ("scope", json!("other")),
                ("backendHandle", json!({"pane":"different"})),
                ("audit", json!([{"id":"other audit"}])),
            ] {
                let mut changed = row.clone();
                changed[key] = value;
                assert!(restore(changed, proof).is_err());
            }
            let mut changed = row.clone();
            changed[NO_SEND_RECOVERY]["claims"]["previous"] = json!({});
            assert!(restore(changed, proof).is_err());
            let (mut recovered, recovered_event) = restore(row, proof)?;
            assert_eq!(recovered_event, event);
            recovered.as_object_mut().unwrap().remove("audit");
            assert_eq!(recovered, before);
        }
        assert_eq!(
            proof_after_refusal(Some(proof), true, "definitely-not-sent"),
            Some(proof)
        );
        assert_eq!(
            proof_after_refusal(Some(proof), false, "definitely-not-sent"),
            None
        );
        assert_eq!(proof_after_refusal(Some(proof), false, "uncertain"), None);
        assert_eq!(proof_after_refusal(Some(proof), true, "uncertain"), None);
        let ordinary = settlement_error(
            anyhow::anyhow!("settlement blocked"),
            None,
            "d-12345678",
            &json!("i-original"),
        );
        assert!(ordinary
            .downcast_ref::<controller::ControllerError>()
            .is_none());
        let released = settlement_error(
            anyhow::anyhow!("settlement blocked"),
            Some(proof),
            "d-12345678",
            &json!("i-original"),
        );
        let released: Value = serde_json::from_str(&released.to_string())?;
        assert_eq!(released["noSendProof"], proof);
        assert_eq!(released["audit"], "i-original");
        Ok(())
    }
}
