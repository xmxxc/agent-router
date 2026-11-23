//! Persist the claim before crossing the Codex queue boundary. Only the exact
//! still-outstanding claim can be settled when the external command returns.
mod codex;
pub(crate) mod queue_owner;
mod registration;
pub use registration::waiters;

use super::{runtime::Monitor, state_dir, Inbox, State};
use crate::workflow::{read_record, valid_id, worker_owner};
use anyhow::{bail, ensure, Context, Result};
use codex::Outcome;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, TryLockError},
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
    stall_after: Duration,
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
        let stall_after = seconds(opts.get("--stall-after").copied().unwrap_or("300"))?;
        Ok(Self {
            scope: opts.get("--scope").copied(),
            thread,
            detach,
            stall_after,
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
        scope_assignments(&state, self.scope)?;
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
            cmd.args(["--stall-after", &self.stall_after.as_secs_f64().to_string()]);
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
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        ensure!(page_size > 0, "cannot determine queue argument limit");
        // Linux MAX_ARG_STRLEN counts the terminating NUL.
        let mut budget = page_size as usize * 32;
        let mut diagnosed = HashSet::new();
        let mut runtime = Monitor::new(self.stall_after);
        while !remaining().is_zero() {
            let order = transport_lock(&state, self.scope)?;
            let batch = match &order {
                Some(_) => match Inbox::try_open(&state)? {
                    Some(mut inbox) => {
                        let verified = scope_assignments(&state, self.scope)?;
                        claim_batch(
                            &mut inbox,
                            self.scope,
                            verified.as_ref(),
                            budget,
                            &mut diagnosed,
                        )?
                    }
                    _ => None,
                },
                None => None,
            };
            if let Some((batch, text)) = batch {
                runtime.cancel_probe();
                let outcome = codex::queue(&thread, &batch, &text, remaining().min(timeout))?;
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
                if outcome == Outcome::TooLong {
                    // exec rejected this exact argument before submission. Its byte
                    // length excludes the NUL, so the next claim must be smaller.
                    budget = text.len();
                }
                if matches!(outcome, Outcome::Delivered | Outcome::TooLong) {
                    continue;
                }
            }
            drop(order);
            if runtime.poll(&state, None, self.scope, remaining())? {
                continue;
            }
            std::thread::sleep(remaining().min(runtime.delay(Duration::from_millis(500))));
        }
        Ok(())
    }
}

// Serialize queue submissions that can overlap, without holding the inbox lock
// across an external call. Scoped senders still progress on independent scopes.
fn transport_lock(state: &Path, scope: Option<&str>) -> Result<Option<Vec<File>>> {
    let mut locks = Vec::new();
    for (name, shared) in std::iter::once(("relay-transport.lock".into(), scope.is_some()))
        .chain(scope.map(|s| (format!("relay-{}.lock", super::body_hash(s)), false)))
    {
        let lock = super::private_file()
            .create(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state.join(name))?;
        match if shared {
            lock.try_lock_shared()
        } else {
            lock.try_lock()
        } {
            Ok(()) => locks.push(lock),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Ok(Some(locks))
}

fn claim_batch(
    inbox: &mut Inbox,
    scope: Option<&str>,
    verified: Option<&Assignments>,
    budget: usize,
    diagnosed: &mut HashSet<u64>,
) -> Result<Option<(Vec<Value>, String)>> {
    let mut messages = Vec::new();
    let mut text = String::from(codex::PREFIX);
    for m in &mut inbox.messages {
        if m.delivery.state != State::Unsent || scope.is_some_and(|s| s != m.worker.scope()) {
            continue;
        }
        if verified.is_some_and(|assignments| {
            !assignments
                .get(&m.worker.dispatch)
                .is_some_and(|(attempt, round)| {
                    attempt == &m.worker.attempt && *round == m.worker.round
                })
        }) {
            continue;
        }
        let view = m.view_at(State::Unknown, m.delivery.next_version()?);
        let envelope = codex::envelope(&view);
        let size = codex::PREFIX.len() + envelope.len() + codex::SUFFIX.len() + 1;
        if size > budget {
            if diagnosed.insert(m.id) {
                println!("router relay: message {} ({}) needs {size} queue argument bytes, limit {budget}; available to wait", m.id, m.message_id);
            }
            continue;
        }
        if !messages.is_empty() {
            if text.len() + 2 + envelope.len() + codex::SUFFIX.len() + 1 > budget {
                break;
            }
            text.push_str(", ");
        }
        text.push_str(&envelope);
        m.claim(State::Unknown)?;
        messages.push(view);
    }
    if messages.is_empty() {
        return Ok(None);
    }
    text.push_str(codex::SUFFIX);
    inbox
        .save()
        .context("claim controller messages for relay")?;
    Ok(Some((messages, text)))
}

type Assignments = HashMap<String, (String, u64)>;

// A truncated legacy reservation has no trustworthy scope or owner. In that
// case a scoped relay can claim only independently verified assignments.
// A global relay has no such target and must defer.
fn scope_assignments(state: &Path, scope: Option<&str>) -> Result<Option<Assignments>> {
    let dir = state.join("dispatches");
    let entries = match fs::read_dir(&dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    let mut ids = HashSet::new();
    for entry in entries {
        let path = entry?.path();
        if matches!(
            path.extension().and_then(OsStr::to_str),
            Some("json" | "pending")
        ) {
            if let Some(id) = path
                .file_stem()
                .and_then(OsStr::to_str)
                .filter(|id| valid_id(id))
            {
                ids.insert(id.to_owned());
            }
        }
    }
    let mut restricted = false;
    let mut verified = Assignments::new();
    for id in ids {
        let mut damaged = false;
        // A concurrent rename can expose both generations. Check both for
        // ownership. A conflict cannot establish a delivery identity.
        let pending = generation(&dir.join(format!("{id}.pending")), &mut damaged)?;
        let published = generation(&dir.join(format!("{id}.json")), &mut damaged)?;
        for row in pending.iter().chain(&published) {
            if !row["controllerOwner"].is_null() {
                let owned = if row["scope"].is_null() {
                    Some(id.as_str())
                } else {
                    row["scope"].as_str()
                };
                ensure!(
                    scope.is_some() && owned.is_some() && scope != owned,
                    "controller owner authorization required for relay"
                );
            }
        }
        if let (Some(a), Some(b)) = (&pending, &published) {
            damaged |= ["attempt", "round", "scope", "controllerOwner"]
                .iter()
                .any(|&k| a[k] != b[k]);
        }
        if damaged {
            restricted = true;
            continue;
        }
        if let Some(row) = published.or(pending) {
            let assigned = if row["scope"].is_null() {
                Some(id.as_str())
            } else {
                row["scope"].as_str()
            };
            if scope.is_some() && scope == assigned && row["controllerOwner"].is_null() {
                if let (Some(attempt), Some(round)) =
                    (row["attempt"].as_str(), row["round"].as_u64())
                {
                    if crate::workflow::valid_identity(attempt, "a-") && round > 0 {
                        verified.insert(id, (attempt.to_owned(), round));
                    }
                }
            }
        }
    }
    Ok(restricted.then_some(verified))
}

// Missing generations are normal during handoff. Malformed contents (including
// a filename/record identity mismatch) supply no evidence for this dispatch.
fn generation(path: &Path, damaged: &mut bool) -> Result<Option<Value>> {
    match read_record(path) {
        Ok(record) => Ok(Some(record)),
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                return if io.kind() == ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(e)
                };
            }
            if e.downcast_ref::<serde_json::Error>()
                .is_some_and(|err| err.is_io())
            {
                return Err(e);
            }
            *damaged = true;
            Ok(None)
        }
    }
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
