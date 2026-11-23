//! Persist the claim before crossing the transport boundary. Only the exact
//! still-outstanding claim can be settled when the transport returns.
mod codex;
mod opencode;
pub(crate) mod queue_owner;
mod registration;
pub use registration::waiters;

use super::{spaced_json, state_dir, Inbox, State};
use crate::workflow::{controller, read_record, valid_id, worker_owner};
use anyhow::{bail, ensure, Context, Result};
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

pub const USAGE: &str = "wfc relay (--codex | --codex-thread UUID | --opencode-url URL --opencode-session ID) [--scope NAME] [--for SECONDS] [--detach] [--stall-after SECONDS] [--owner-token TOKEN]";

pub struct Relay<'a> {
    scope: Option<&'a str>,
    target: Target<'a, Option<&'a str>>,
    detach: bool,
    lifetime: Option<Duration>,
    owner_token: Option<&'a str>,
    stall_after: Duration,
}

enum Target<'a, Thread> {
    Codex(Thread),
    OpenCode(opencode::Target<'a>),
}

impl<'a> Target<'a, Option<&'a str>> {
    fn resolve(self) -> Result<Target<'a, String>> {
        Ok(match self {
            Target::Codex(thread) => {
                let current = env::var("CODEX_THREAD_ID").ok();
                Target::Codex(codex::controller(
                    thread
                        .or(current.as_deref())
                        .filter(|s| !s.is_empty())
                        .context("--codex needs CODEX_THREAD_ID")?,
                )?)
            }
            Target::OpenCode(target) => Target::OpenCode(target),
        })
    }
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
                "--codex-thread" | "--opencode-url" | "--opencode-session" | "--scope"
                | "--for" | "--stall-after" | "--owner-token" => {
                    let v = args
                        .next()
                        .and_then(|v| v.to_str())
                        .filter(|v| {
                            (!v.is_empty() || flag == "--owner-token") && !v.starts_with("--")
                        })
                        .context("missing relay option value")?;
                    ensure!(opts.insert(flag, v).is_none(), "duplicate {flag}");
                }
                _ => bail!("unknown or duplicate relay option {flag}"),
            }
        }
        let thread = opts.get("--codex-thread").copied();
        let url = opts.get("--opencode-url").copied();
        let session = opts.get("--opencode-session").copied();
        let target = match (codex, thread, url, session) {
            (true, None, None, None) | (false, Some(_), None, None) => Target::Codex(thread),
            (false, None, Some(url), Some(session)) => Target::OpenCode(opencode::Target::parse(url, session)?),
            _ => bail!("give exactly one controller: --codex, --codex-thread, or --opencode-url with --opencode-session"),
        };
        let stall_after = seconds(opts.get("--stall-after").copied().unwrap_or("300"))?;
        Ok(Self {
            scope: opts.get("--scope").copied(),
            target,
            detach,
            lifetime: opts
                .get("--for")
                .map(|v| seconds(v))
                .transpose()?
                .filter(|d| !d.is_zero()),
            owner_token: opts.get("--owner-token").copied(),
            stall_after,
        })
    }

    pub fn run(self) -> Result<()> {
        let state = state_dir()?;
        let token = controller::token(self.owner_token);
        let preflight =
            || controller::Owners::read(&state)?.preflight(self.scope, token.as_deref(), "relay");
        preflight()?;
        scope_assignments(&state, self.scope)?;
        let target = self.target.resolve()?;
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
            cmd.arg("relay");
            match &target {
                Target::Codex(thread) => cmd.args(["--codex-thread", thread]),
                Target::OpenCode(target) => cmd.args([
                    "--opencode-url",
                    target.url,
                    "--opencode-session",
                    target.session,
                ]),
            };
            if let Some(scope) = self.scope {
                cmd.args(["--scope", scope]);
            }
            if let Some(limit) = self.lifetime {
                cmd.args(["--for", &limit.as_secs_f64().to_string()]);
            }
            cmd.args(["--stall-after", &self.stall_after.as_secs_f64().to_string()]);
            if let Some(token) = &token {
                cmd.env("WFC_CONTROLLER_TOKEN", token)
                    .env_remove("WFC_OWNER_TOKEN");
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
        let destination = match &target {
            Target::Codex(thread) => ("thread", thread.as_str()),
            Target::OpenCode(target) => ("session", target.session),
        };
        let _entry = registration::Registration::create(&state, self.scope, destination)?;
        let start = Instant::now();
        let remaining = || {
            self.lifetime
                .map_or(Duration::MAX, |d| d.saturating_sub(start.elapsed()))
        };
        let mut budget = match &target {
            Target::Codex(_) => {
                let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                ensure!(page_size > 0, "cannot determine queue argument limit");
                // Linux MAX_ARG_STRLEN counts the terminating NUL.
                page_size as usize * 32
            }
            Target::OpenCode(_) => usize::MAX, // The batch is an HTTP body, not an argument.
        };
        let mut diagnosed = HashSet::new();
        let mut runtime = super::runtime::Monitor::new(self.stall_after);
        while !remaining().is_zero() {
            preflight()?;
            let order = transport_lock(&state, self.scope)?;
            let batch = match &order {
                Some(_) => match Inbox::try_open(&state)? {
                    Some(mut inbox) => {
                        preflight()?;
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
                let limit = remaining().min(timeout);
                let outcome = match &target {
                    Target::Codex(thread) => {
                        let scope_lock = order
                            .as_ref()
                            .and_then(|(_, scope_lock)| scope_lock.as_ref());
                        codex::queue(thread, &batch, &text, limit, scope_lock)?
                    }
                    Target::OpenCode(target) => target.deliver(&text, limit),
                };
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
                if let Target::Codex(thread) = &target {
                    ensure!(
                        outcome != Outcome::Missing,
                        "no rollout found for thread {thread}; messages stay waiting"
                    );
                }
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
            if runtime.poll(&state, None, self.scope, token.as_deref(), remaining())? {
                continue;
            }
            std::thread::sleep(remaining().min(runtime.delay(Duration::from_millis(500))));
        }
        Ok(())
    }
}

// Serialize queue submissions that can overlap, without holding the inbox lock
// across an external call. Scoped senders still progress on independent scopes.
fn transport_lock(state: &Path, scope: Option<&str>) -> Result<Option<(File, Option<File>)>> {
    let transport = super::private_file()
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(state.join("relay-transport.lock"))?;
    match if scope.is_some() {
        transport.try_lock_shared()
    } else {
        transport.try_lock()
    } {
        Ok(()) => (),
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    if let Some(scope) = scope {
        return Ok(controller::try_scope_lock(state, scope)?.map(|lock| (transport, Some(lock))));
    }
    Ok(Some((transport, None)))
}

// Match the Python queue text, including UTF-8, JSON separators and field order.
const PREFIX: &str = "[router] Messages for the controller: {\"messages\": [";
const SUFFIX: &str = "]}";

fn envelope(m: &Value) -> String {
    let header = [
        "id",
        "messageId",
        "dispatch",
        "type",
        "state",
        "statusVersion",
    ];
    let mut fields = Vec::new();
    let mut field = |k: &str, v: &Value| fields.push(format!("{}: {}", json!(k), spaced_json(v)));
    for k in header {
        field(k, &m[k]);
    }
    for (k, v) in m.as_object().expect("controller message").iter() {
        if !header.contains(&k.as_str()) {
            field(k, v);
        }
    }
    field("delivery", &json!("pending"));
    field(
        "deliveryAttempt",
        &json!(format!(
            "relay:{}:{}",
            m["messageId"].as_str().unwrap(),
            m["statusVersion"]
        )),
    );
    format!("{{{}}}", fields.join(", "))
}

// Discriminants also serve as private exit codes for the Codex queue supervisor.
#[derive(PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Outcome {
    Delivered = 0,
    Failed = 20,
    Missing = 21,
    Uncertain = 22,
    TooLong = 23,
}

fn claim_batch(
    inbox: &mut Inbox,
    scope: Option<&str>,
    verified: Option<&Assignments>,
    budget: usize,
    diagnosed: &mut HashSet<u64>,
) -> Result<Option<(Vec<Value>, String)>> {
    let mut messages = Vec::new();
    let mut text = String::from(PREFIX);
    for m in &mut inbox.messages {
        if !m.is_controller_message()
            || m.delivery.state != State::Unsent
            || scope.is_some_and(|s| s != m.worker.scope())
        {
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
        let envelope = envelope(&view);
        let size = PREFIX.len() + envelope.len() + SUFFIX.len() + 1;
        if size > budget {
            if diagnosed.insert(m.id) {
                println!("router relay: message {} ({}) needs {size} queue argument bytes, limit {budget}; available to wait", m.id, m.message_id);
            }
            continue;
        }
        if !messages.is_empty() {
            if text.len() + 2 + envelope.len() + SUFFIX.len() + 1 > budget {
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
    text.push_str(SUFFIX);
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
