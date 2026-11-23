//! Read-only evidence for the shared lifecycle reconciliation transaction.
use super::super::{assignment, record_scope, valid_id};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

/// The caller authorizes the scope, holds the attempt lock, and supplies the
/// unprojected current row. Before committing, it rechecks ownership and the
/// row under its transaction locks. None leaves the create unresolved.
#[allow(
    dead_code,
    reason = "the shared #18 reconcile transaction is integrated separately"
)]
pub(in crate::workflow) fn cleanup_receipt(
    state: &Path,
    row: &Value,
    audit: u64,
) -> Result<Option<Value>> {
    let Some(dispatch) = row["dispatch"].as_str().filter(|id| valid_id(id)) else {
        return Ok(None);
    };
    let Some(scope) = record_scope(row).filter(|scope| !scope.is_empty()) else {
        return Ok(None);
    };
    if audit == 0 || row["backend"] != "bundled" || !assignment::identified(row) {
        return Ok(None);
    }
    let Some(audits) = row["audit"].as_array() else {
        return Ok(None);
    };
    let mut events = audits.iter().filter(|event| {
        event["id"].as_u64() == Some(audit)
            || (event["operation"] == "create"
                && event["attempt"] == row["attempt"]
                && event["round"] == row["round"])
    });
    let Some(event) = events.next() else {
        return Ok(None);
    };
    if event["id"].as_u64() != Some(audit)
        || events.next().is_some()
        || event["operation"] != "create"
        || event["dispatch"] != dispatch
        || event["round"] != row["round"]
        || event["attempt"] != row["attempt"]
        || (!event["toAttempt"].is_null() && event["toAttempt"] != row["attempt"])
        || (!event["scope"].is_null() && event["scope"] != scope)
        || !row["host"].is_string()
        || event["toHost"] != row["host"]
        || !matches!(
            event["result"].as_str(),
            Some("pending" | "uncertain" | "unknown")
        )
    {
        return Ok(None);
    }

    let create = &row["create"];
    let ticket = &create["ticket"];
    if row["createPending"] == true
        || ticket["version"] != 2
        || ticket["unit"] != dispatch
        || ticket["attempt"] != row["attempt"]
        || (!row["backendHandle"].is_null() && &row["backendHandle"] != ticket)
        || ["dispatch", "round", "attempt"]
            .iter()
            .any(|&key| !create[key].is_null() && create[key] != row[key])
        || assignment::public_record(&state.join("dispatches"), dispatch)?.as_ref() != Some(row)
    {
        return Ok(None);
    }

    let Some(proof) =
        crate::bundled::registered_cleanup_receipt(state, ticket, &create["identity"])
    else {
        return Ok(None);
    };
    Ok(Some(json!({
        "dispatch":dispatch,"scope":scope,"round":row["round"],"attempt":row["attempt"],
        "audit":audit,"proof":proof
    })))
}

#[cfg(test)]
mod tests;
