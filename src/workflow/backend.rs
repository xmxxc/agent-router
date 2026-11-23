//! Public lifecycle client for the tmux create/inspect/close protocol (#31).
use super::{host::Launch, valid_identity, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(target_os = "linux")]
pub fn tmux_socket() -> Result<PathBuf> {
    let socket = env::var_os("WFC_TMUX_SOCKET")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var("TMUX")
                .ok()
                .and_then(|v| v.rsplitn(3, ',').nth(2).map(PathBuf::from))
        })
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into()))
                .join(format!("tmux-{}", unsafe { libc::getuid() }))
                .join("default")
        });
    std::path::absolute(socket).context("resolve tmux socket")
}

fn call(state: &Path, row: &Value, mut request: Value) -> Result<Value> {
    ensure!(
        row["backend"] == "tmux",
        "{} backend adapter is unavailable in this build",
        row["backend"]
    );
    request["dispatch"] = row["dispatch"].clone();
    let mut command = Command::new(env::current_exe()?);
    command.arg("--tmux-backend").env("WFC_STATE", state);
    if let Some(socket) = row["tmuxSocket"].as_str() {
        command.env("WFC_TMUX_SOCKET", socket);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let sent = serde_json::to_writer(
        child.stdin.take().context("missing backend input")?,
        &request,
    );
    // Once the pipe has been offered the request, a failed wait/read cannot
    // establish that the backend did nothing. Keep the caller's assignment.
    let out = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => {
            return Ok(
                json!({"result":"uncertain","diagnostic":format!("could not collect tmux backend response: {e}")}),
            )
        }
    };
    if out.status.code() == Some(2)
        && out.stdout.is_empty()
        && out.stderr.strip_suffix(b"\n") == Some(crate::UNEXPECTED_ARGUMENTS.as_bytes())
    {
        bail!("tmux backend is unavailable in this build (requires #31)");
    }
    if sent.is_err() || !out.status.success() {
        return Ok(
            json!({"result":"uncertain","diagnostic":"tmux backend did not confirm the request"}),
        );
    }
    let reply: Option<Value> = serde_json::from_slice(&out.stdout).ok();
    match reply {
        Some(reply)
            if matches!(
                reply["result"].as_str(),
                Some("accepted" | "refused" | "definitely-not-sent" | "uncertain")
            ) || (request["op"] == "close" && reply["result"] == "completed") =>
        {
            Ok(reply)
        }
        _ => Ok(json!({"result":"uncertain","diagnostic":"invalid tmux backend response"})),
    }
}

pub fn create(state: &Path, row: &Value, launch: Launch) -> Result<Value> {
    call(
        state,
        row,
        json!({"op":"create","cwd":row["cwd"],"label":row["label"],"argv":launch.argv,"env":launch.env}),
    )
}

fn existing(state: &Path, row: &Value, op: &str) -> Result<Value> {
    let attempt = row["attempt"]
        .as_str()
        .filter(|v| valid_identity(v, "a-"))
        .context("missing existing execution identity")?;
    let launch = receipt_id(row)?;
    let mut request = json!({"op":op});
    // A new native create with a lost response already reserved this unique
    // dispatch in #31. Let the provider look up that registration. Supplying
    // the outer attempt as a legacy ID would select a different execution.
    let legacy = row["create"]
        .is_null()
        .then(|| row["legacyExecutionId"].as_str().unwrap_or(attempt));
    if let Some(id) = launch.or(legacy) {
        request["legacyExecutionId"] = json!(id);
    }
    if let Some(pane) = row.get("legacyWorkerPane") {
        request["legacyWorkerPane"] = pane.clone();
    }
    call(state, row, request)
}

fn receipt_id(row: &Value) -> Result<Option<&str>> {
    let create = &row["create"];
    if !create.is_null() {
        // R2 only created first-round assignments and stored the reply inline.
        let r2 = row["round"] == 1
            && ["dispatch", "attempt", "round"]
                .iter()
                .all(|k| create.get(k).is_none());
        ensure!(
            r2 || ["dispatch", "attempt", "round"]
                .iter()
                .all(|k| create[*k] == row[*k]),
            "launch evidence does not match this attempt"
        );
    }
    let Some(name) = create["receipt"].as_str() else {
        return Ok(None);
    };
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid launch receipt")?;
    ensure!(
        stem.len() == 32
            && stem.bytes().all(|b| b.is_ascii_hexdigit())
            && path.extension() == Some("json".as_ref()),
        "invalid launch receipt"
    );
    Ok(Some(stem))
}

fn private_receipt(state: &Path, row: &Value) -> Result<Value> {
    let Some(stem) = receipt_id(row)? else {
        return Ok(Value::Null);
    };
    match File::open(state.join("launches").join(format!("{stem}.json"))) {
        Ok(f) => Ok(serde_json::from_reader(f)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e).context("open host launch receipt"),
    }
}

fn closed_path(state: &Path, row: &Value) -> Result<PathBuf> {
    let attempt = row["attempt"]
        .as_str()
        .filter(|v| valid_identity(v, "a-"))
        .context("invalid attempt")?;
    Ok(state.join("dispatches").join(format!(
        "{}.{attempt}.closed",
        row["dispatch"].as_str().context("invalid dispatch")?
    )))
}

fn archive(state: &Path, row: &Value) -> Result<Option<Value>> {
    match File::open(closed_path(state, row)?) {
        Ok(f) => {
            let saved: Value = serde_json::from_reader(f)?;
            ensure!(
                saved["dispatch"] == row["dispatch"]
                    && saved["attempt"] == row["attempt"]
                    && saved["round"] == row["round"],
                "closed record identity mismatch"
            );
            Ok(Some(saved))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn archived_exit(saved: &Value) -> bool {
    let worker = &saved["worker"];
    worker["state"] == "exited" && worker["exit_code"].as_u64().is_some_and(|code| code <= 255)
}

fn confirmed(saved: &Value) -> bool {
    // R2 could archive cancellation while a server was lost. An old process
    // exit code is still evidence about the worker, not proof of session cleanup.
    match saved["closeEffect"].as_str() {
        Some("already_absent" | "worker_exited") => archived_exit(saved),
        Some("session_closed") => archived_exit(saved) || saved["worker"]["state"] == "cancelled",
        _ => false,
    }
}

fn exited(evidence: &Value) -> Option<Value> {
    let code = evidence["exitCode"].as_u64().filter(|n| *n <= 255)?;
    (evidence["category"] == "process_exited")
        .then(|| json!({"state":"exited", "exit_code":code, "signal":null}))
}

pub fn observe(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let saved = archive(state, row)?.unwrap_or(Value::Null);
    if confirmed(&saved) {
        return Ok((
            saved["worker"].clone(),
            json!({"result":"completed","alive":false}),
        ));
    }
    let mut inspection = existing(state, row, "inspect")
        .unwrap_or_else(|e| json!({"result":"uncertain","diagnostic":e.to_string()}));
    // With a lost create reply, an empty lookup alone does not prove that
    // the native request has finished. It may still be reserving its launch.
    if row["create"]["result"] == "uncertain"
        && row["create"]["receipt"].is_null()
        && inspection["alive"] == false
    {
        inspection = json!({"result":"uncertain","diagnostic":"missing confirmation of the original native launch"});
    }
    // inspect may take long enough for the host to exit. Sample the receipt
    // belonging to this assignment after that observation and before any kill.
    let evidence = private_receipt(state, row)?;
    let worker = if let Some(exit) = exited(&evidence) {
        exit
    } else if archived_exit(&saved) {
        saved["worker"].clone()
    } else if matches!(evidence["category"].as_str(), Some("started") | None)
        && inspection["alive"] == true
        && inspection["owned"] == true
    {
        json!({"state":"running"})
    } else {
        json!({"state":"unknown"})
    };
    Ok((worker, inspection))
}

pub fn close(state: &Path, row: &Value) -> Result<Value> {
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    // This immutable assignment file serializes competing closes, including
    // across a restart. Completed archives need only read access.
    let lock = File::open(state.join("dispatches").join(format!("{id}.json")))?;
    lock.lock()?;
    let mut receipt = json!({"dispatch":id,"operation":"close","result":"completed","round":row["round"],"attempt":row["attempt"]});
    if let Some(saved) = archive(state, row)?.filter(confirmed) {
        if let Some(effect) = saved.get("closeEffect") {
            receipt["closeEffect"] = effect.clone();
        }
        receipt["diagnostic"] = json!("session was already gone");
        return Ok(receipt);
    }
    let (before, observed) = observe(state, row)?;
    ensure!(
        observed["result"] == "accepted",
        "backend inspection {}: {}",
        observed["result"],
        observed["diagnostic"]
    );
    if observed["alive"] == true {
        ensure!(
            observed["owned"] == true,
            "tmux session is not owned by this dispatch"
        );
    }
    let answer = existing(state, row, "close")?;
    ensure!(
        answer["result"] == "completed",
        "close {}: {}",
        answer["result"],
        answer["diagnostic"]
    );
    // Only the service performing the guarded close knows whether it killed
    // a live session. Its observation can be later than our last inspection.
    let worker = match answer["closeEffect"].as_str() {
        Some("already_absent" | "worker_exited") => {
            if before["state"] == "exited" {
                before
            } else {
                exited(&private_receipt(state, row)?)
                    .context("close uncertain: missing same-attempt worker exit evidence")?
            }
        }
        Some("session_closed") if before["state"] == "exited" => before,
        Some("session_closed") => json!({"state":"cancelled","exit_code":null,"signal":null}),
        _ => bail!("close uncertain: backend did not confirm the close effect"),
    };
    if answer["closeEffect"] == "already_absent" {
        receipt["diagnostic"] = json!("session was already gone");
    }
    receipt["closeEffect"] = answer["closeEffect"].clone();
    let snapshot = json!({"dispatch":id,"round":row["round"],"attempt":row["attempt"],
        "worker":worker,"closeEffect":answer["closeEffect"]});
    let path = closed_path(state, row)?;
    write_json(&path, &snapshot, None)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_evidence_belongs_to_the_current_attempt() {
        let stem = "0123456789abcdef0123456789abcdef";
        let mut row = json!({"dispatch":"d-12345678","attempt":"a-123456789abc","round":1,
            "create":{"receipt":format!("launches/{stem}.json")}});
        assert_eq!(receipt_id(&row).unwrap(), Some(stem)); // first-round R2 archive
        row["round"] = json!(2);
        assert!(receipt_id(&row).is_err());
        for field in ["dispatch", "attempt", "round"] {
            row["create"][field] = row[field].clone();
        }
        assert_eq!(receipt_id(&row).unwrap(), Some(stem));
        row["create"]["attempt"] = json!("a-23456789abcd");
        assert!(receipt_id(&row).is_err());
        assert!(exited(&json!({"category":"process_exited"})).is_none());
        assert!(exited(&json!({"category":"started","exitCode":37})).is_none());
    }
}
