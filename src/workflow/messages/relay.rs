//! Persist the claim before crossing the Codex queue boundary. Only the exact
//! still-outstanding claim can be settled when the external command returns.
mod codex;
mod registration;
pub use registration::waiters;

use super::{state_dir, Inbox, State};
use crate::workflow::{read_record, valid_id, worker_owner};
use anyhow::{bail, ensure, Context, Result};
use codex::Outcome;
use serde_json::{json, Value};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::ErrorKind,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const USAGE: &str = "wfc relay (--codex | --codex-thread UUID) [--scope NAME] [--for SECONDS] [--detach] [--stall-after SECONDS] [--owner-token TOKEN]";

pub struct Relay<'a> {
    scope: Option<&'a str>,
    thread: Option<&'a str>,
    detach: bool,
    lifetime: Option<Duration>,
}

fn seconds(value: &str) -> Result<Duration> {
    let seconds: f64 = value.parse()?;
    let duration =
        Duration::try_from_secs_f64(seconds).context("expected finite nonnegative seconds")?;
    // Exact zero means continuous for --for, including after a detached re-exec.
    Ok(if seconds > 0.0 {
        duration.max(Duration::from_nanos(1))
    } else {
        duration
    })
}

impl<'a> Relay<'a> {
    pub fn parse(args: &'a [OsString]) -> Result<Self> {
        let mut codex = false;
        let mut detach = false;
        let mut opts = std::collections::BTreeMap::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let flag = arg.to_str().context("invalid relay option")?;
            match flag {
                "--codex" if !codex => codex = true,
                "--detach" if !detach => detach = true,
                "--codex-thread" | "--scope" | "--for" | "--stall-after" | "--owner-token" => {
                    let v = args
                        .next()
                        .and_then(|v| v.to_str())
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))
                        .context("missing relay option value")?;
                    ensure!(opts.insert(flag, v).is_none(), "duplicate {flag}");
                }
                _ => bail!("unknown or duplicate relay option {flag}"),
            }
        }
        let thread = opts.get("--codex-thread").copied();
        ensure!(codex != thread.is_some(), "give --codex or --codex-thread");
        if let Some(value) = opts.get("--stall-after") {
            seconds(value)?;
        }
        Ok(Self {
            scope: opts.get("--scope").copied(),
            thread,
            detach,
            lifetime: opts
                .get("--for")
                .map(|v| seconds(v))
                .transpose()?
                .filter(|d| !d.is_zero()),
        })
    }

    pub fn run(self) -> Result<()> {
        ensure!(
            env::var_os("WFC_WORKER_CALLBACK").is_none_or(|v| v.is_empty()),
            "worker_callback_only: relay"
        );
        let state = state_dir()?;
        scope_ready(&state, self.scope)?;
        let current = env::var("CODEX_THREAD_ID").ok();
        let thread = codex::controller(
            self.thread
                .or(current.as_deref())
                .filter(|s| !s.is_empty())
                .context("--codex needs CODEX_THREAD_ID")?,
        )?;
        let timeout = seconds(&env::var("WFC_QUEUE_TIMEOUT").unwrap_or_else(|_| "60".into()))?;
        if self.detach {
            fs::create_dir_all(&state)?;
            let log = state.join("relay.log");
            let out = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&log)?;
            ensure!(out.metadata()?.is_file(), "invalid relay log");
            let mut cmd = Command::new(env::current_exe()?);
            cmd.args(["relay", "--codex-thread", &thread]);
            if let Some(scope) = self.scope {
                cmd.args(["--scope", scope]);
            }
            if let Some(limit) = self.lifetime {
                cmd.args(["--for", &limit.as_secs_f64().to_string()]);
            }
            worker_owner::new_session(&mut cmd);
            let child = cmd
                .stdin(Stdio::null())
                .stdout(out.try_clone()?)
                .stderr(out)
                .spawn()?;
            println!("{}", json!({"pid":child.id(),"log":log}));
            return Ok(());
        }
        let _entry = registration::Registration::create(&state, self.scope, &thread)?;
        let start = Instant::now();
        let remaining = || {
            self.lifetime
                .map_or(Duration::MAX, |d| d.saturating_sub(start.elapsed()))
        };
        while !remaining().is_zero() {
            let batch = if let Some(mut inbox) = Inbox::try_open(&state)? {
                if scope_ready(&state, self.scope)? {
                    inbox.claim(None, self.scope, State::Unknown)?
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            if !batch.is_empty() {
                let outcome = codex::queue(&thread, &batch, remaining().min(timeout))?;
                if outcome != Outcome::Uncertain {
                    loop {
                        if let Some(mut inbox) = Inbox::try_open(&state)? {
                            settle(&mut inbox, &batch, outcome == Outcome::Delivered)?;
                            break;
                        }
                        ensure!(
                            !remaining().is_zero(),
                            "timeout settling queue result; claim remains unknown"
                        );
                        std::thread::sleep(remaining().min(Duration::from_millis(10)));
                    }
                }
                ensure!(
                    outcome != Outcome::Missing,
                    "no rollout found for thread {thread}; messages stay waiting"
                );
            }
            std::thread::sleep(remaining().min(Duration::from_millis(500)));
        }
        Ok(())
    }
}

// Until owner leases are integrated, refuse claimed scopes, even for a global
// relay. An unrelated scope remains eligible. Dispatch publication uses rename.
fn scope_ready(state: &Path, scope: Option<&str>) -> Result<bool> {
    let entries = match fs::read_dir(state.join("dispatches")) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(true),
        other => other?,
    };
    for entry in entries {
        let path = entry?.path();
        if !matches!(
            path.extension().and_then(OsStr::to_str),
            Some("json" | "pending")
        ) || !path
            .file_stem()
            .and_then(OsStr::to_str)
            .is_some_and(valid_id)
        {
            continue;
        }
        let pending = path.extension() == Some(OsStr::new("pending"));
        let missing = |e: &anyhow::Error| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == ErrorKind::NotFound)
        };
        let record = match read_record(&path) {
            // The launcher is still writing its reservation. Defer this poll
            // until that scope can be inspected, without terminating the relay.
            Err(e)
                if pending
                    && e.downcast_ref::<serde_json::Error>()
                        .is_some_and(|e| e.is_eof()) =>
            {
                return Ok(false)
            }
            Err(e) if pending && missing(&e) => match read_record(&path.with_extension("json")) {
                Err(e) if missing(&e) => continue, // failed launch rolled back
                other => other?,
            },
            other => other?,
        };
        if !record["controllerOwner"].is_null() {
            let owned = record["scope"].as_str().or(record["dispatch"].as_str());
            ensure!(
                scope.is_some() && scope != owned,
                "controller owner authorization required for relay"
            );
        }
    }
    Ok(true)
}

fn settle(inbox: &mut Inbox, batch: &[Value], received: bool) -> Result<()> {
    let mut changed = false;
    for (m, claim) in inbox.messages.iter_mut().filter_map(|m| {
        batch
            .binary_search_by_key(&m.id, |v| v["id"].as_u64().unwrap())
            .ok()
            .map(|i| (m, &batch[i]))
    }) {
        if m.delivery.state == State::Unknown
            && claim["statusVersion"] == m.delivery.status_version
            && claim["messageId"] == m.message_id
            && claim["dispatch"] == m.worker.dispatch
        {
            m.delivery.advance(if received {
                State::Received
            } else {
                State::Unsent
            })?;
            changed = true;
        }
    }
    if changed {
        inbox.save()?;
    }
    Ok(())
}
