//! Public host lifecycle and launch receipts for Linux backend adapters.
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
    let out = child.wait_with_output()?;
    if out.status.code() == Some(2) {
        bail!("tmux backend is unavailable in this build (requires #31)");
    }
    if sent.is_err() || !out.status.success() {
        return Ok(
            json!({"result":"uncertain","diagnostic":"tmux backend did not confirm the request"}),
        );
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or_else(
        |_| json!({"result":"uncertain","diagnostic":"invalid tmux backend response"}),
    ))
}

pub fn create(state: &Path, row: &Value, launch: Launch) -> Result<Value> {
    call(
        state,
        row,
        json!({"op":"create","cwd":row["cwd"],"label":row["label"],"argv":launch.argv,"env":launch.env}),
    )
}

pub(super) fn private_receipt(state: &Path, create: &Value) -> Result<Value> {
    let Some(name) = create["receipt"].as_str() else {
        return Ok(Value::Null);
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

pub(super) fn closed(state: &Path, row: &Value) -> Result<Option<Value>> {
    match File::open(closed_path(state, row)?) {
        Ok(f) => {
            let saved: Value = serde_json::from_reader(f)?;
            ensure!(
                saved["dispatch"] == row["dispatch"]
                    && saved["attempt"] == row["attempt"]
                    && saved["round"] == row["round"],
                "closed record identity mismatch"
            );
            Ok(Some(saved["worker"].clone()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn observe(state: &Path, row: &mut Value) -> Result<(Value, Value)> {
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::observe(state, row);
    }
    if let Some(result) = closed(state, row)? {
        return Ok((result, json!({"result":"completed","alive":false})));
    }
    let inspection = inspect_tmux(state, row);
    let launch = private_receipt(state, &row["create"])?;
    Ok((worker(&launch, &inspection), inspection))
}

fn inspect_tmux(state: &Path, row: &Value) -> Value {
    let mut inspection = call(state, row, json!({"op":"inspect"}))
        .unwrap_or_else(|e| json!({"result":"uncertain","diagnostic":e.to_string()}));
    // An unbound legacy row can find a positively owned live pane on today's
    // socket. Absence on that socket alone cannot prove the old session ended.
    if row["tmuxSocket"].is_null() && inspection["alive"] == false {
        inspection = json!({"result":"uncertain","diagnostic":"original tmux socket is unknown"});
    }
    inspection
}

pub(super) fn worker(evidence: &Value, inspection: &Value) -> Value {
    match evidence["category"].as_str() {
        Some("process_exited") => {
            json!({"state":"exited","exit_code":evidence["exitCode"],"signal":null})
        }
        Some("started") | None if inspection["alive"] == true && inspection["owned"] == true => {
            json!({"state":"running"})
        }
        _ => json!({"state":"unknown"}),
    }
}

pub fn close(state: &Path, row: &Value) -> Result<Value> {
    #[cfg(target_os = "linux")]
    if row["backend"] == "herdr" {
        return super::herdr::close(state, row);
    }
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    // This immutable assignment file serializes competing closes, including
    // across a restart. Completed archives need only read access.
    let lock = File::open(state.join("dispatches").join(format!("{id}.json")))?;
    lock.lock()?;
    let receipt = close_locked(state, row, |op| {
        if op == "inspect" {
            Ok(inspect_tmux(state, row))
        } else {
            call(state, row, json!({"op":op}))
        }
    })?;
    ensure!(
        receipt["result"] == "completed",
        "close {}: {}",
        receipt["result"],
        receipt["diagnostic"]
    );
    Ok(receipt)
}

pub(super) fn close_locked(
    state: &Path,
    row: &Value,
    mut operation: impl FnMut(&str) -> Result<Value>,
) -> Result<Value> {
    let id = &row["dispatch"];
    let mut receipt = json!({"dispatch":id,"operation":"close","result":"completed","round":row["round"],"attempt":row["attempt"]});
    if closed(state, row)?.is_some() {
        receipt["diagnostic"] = json!("session was already gone");
        return Ok(receipt);
    }
    let observed = operation("inspect")?;
    if observed["result"] != "accepted" {
        receipt["result"] = observed["result"].clone();
        receipt["diagnostic"] = observed["diagnostic"].clone();
        return Ok(receipt);
    }
    if observed["alive"] == true && observed["owned"] != true {
        receipt["result"] = json!("refused");
        receipt["diagnostic"] = json!("session is not owned by this dispatch");
        return Ok(receipt);
    }
    let before = worker(&private_receipt(state, &row["create"])?, &observed);
    let outcome = operation("close")?;
    if outcome["result"] != "completed" {
        receipt["result"] = outcome["result"].clone();
        receipt["diagnostic"] = outcome["diagnostic"].clone();
        return Ok(receipt);
    }
    // The close operation itself can cause an exit receipt. Retain a
    // previously observed terminal outcome; confirmed closure cancels any
    // remaining work. A completed business round is projected separately.
    let worker = if before["state"] == "exited" {
        before
    } else {
        json!({"state":"cancelled","exit_code":null,"signal":null})
    };
    if observed["alive"] == false {
        receipt["diagnostic"] = json!("session was already gone");
    }
    let snapshot =
        json!({"dispatch":id,"round":row["round"],"attempt":row["attempt"],"worker":worker});
    let path = closed_path(state, row)?;
    write_json(&path, &snapshot, None)?;
    Ok(receipt)
}
