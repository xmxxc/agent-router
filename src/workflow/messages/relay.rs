//! Persist the claim before crossing the transport boundary. Only the exact
//! still-outstanding claim can be settled when the transport returns.
mod codex;
mod codex_backoff;
mod opencode;
pub(crate) mod queue_owner;
mod registration;
pub use registration::waiters;

use super::{
    message_boundary,
    recovery::{GenerationStamps, Generations, Scan, Stamp},
    spaced_json, Identity, Inbox, State,
};
use crate::workflow::{
    assignment::MessageBoundary, controller, ensure_state_writable, worker_owner,
    writable_state_dir,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs::{self, File, TryLockError},
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
    stall: Option<Duration>,
    owner_token: Option<&'a str>,
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
        Ok(Self {
            scope: opts.get("--scope").copied(),
            target,
            detach,
            stall: opts.get("--stall-after").map(|v| seconds(v)).transpose()?,
            lifetime: opts
                .get("--for")
                .map(|v| seconds(v))
                .transpose()?
                .filter(|d| !d.is_zero()),
            owner_token: opts.get("--owner-token").copied(),
        })
    }

    pub fn run(self) -> Result<()> {
        let state = writable_state_dir()?;
        let token = controller::token(self.owner_token);
        let preflight = || {
            ensure_state_writable(&state)?;
            controller::Owners::read(&state)?.preflight(self.scope, token.as_deref(), "relay")
        };
        preflight()?;
        let target = self.target.resolve()?;
        let timeout = seconds(&env::var("WFC_QUEUE_TIMEOUT").unwrap_or_else(|_| "60".into()))?;
        // Legacy authorization is scope-wide. Complete it outside all
        // transport locks before starting the delivery window.
        let mut assignments = scope_assignments(&state, self.scope, None, || false)?
            .context("initial relay authorization did not finish")?;
        ensure_state_writable(&state)?;
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
            if let Some(stall) = self.stall {
                cmd.args(["--stall-after", &stall.as_secs_f64().to_string()]);
            }
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
        let backoff = match &target {
            Target::Codex(thread) => Some(codex_backoff::Backoff::new(&state, thread)?),
            Target::OpenCode(_) => None,
        };
        let mut scan = Scan::default();
        let mut watch = super::runtime_fault::Watch::default();
        let (max_turn, stall) = (
            crate::workflow::runtime_fault::max_turn(),
            self.stall
                .unwrap_or_else(crate::workflow::runtime_fault::stall_after),
        );
        let mut observations = Vec::new();
        let mut recovery = Vec::new();
        let mut first = true;
        while !remaining().is_zero() {
            preflight()?;
            let was_first = std::mem::replace(&mut first, false);
            if !was_first {
                if recovery.is_empty() {
                    recovery = scan.batch(&state, None, self.scope, remaining());
                }
                if observations.is_empty() {
                    observations = watch.poll(&state, None, self.scope, remaining());
                }
                let Some(current) =
                    scope_assignments(&state, self.scope, Some(assignments), || {
                        remaining().is_zero()
                    })?
                else {
                    break;
                };
                assignments = current;
            }
            if remaining().is_zero() {
                break;
            }
            let order = transport_lock(&state, self.scope)?;
            let target_lock = match (&backoff, &order) {
                (Some(backoff), Some(_)) => backoff.try_ready()?,
                _ => None,
            };
            let batch = match &order {
                Some(_)
                    if assignments.current_before_claim(&state, || remaining().is_zero())? =>
                {
                    match Inbox::try_open(&state)? {
                        Some(mut inbox) if assignments.namespace_current(&state)? => {
                            preflight()?;
                            if self.scope.is_some() || !assignments.restricted {
                                inbox.recover_startups(&mut scan, std::mem::take(&mut recovery))?;
                                inbox.publish_runtime(
                                    std::mem::take(&mut observations),
                                    max_turn,
                                    stall,
                                )?;
                            } else {
                                recovery.clear();
                                observations.clear();
                            }
                            if backoff.is_some() && target_lock.is_none() {
                                None
                            } else {
                                claim_batch(
                                    &mut inbox,
                                    self.scope,
                                    &assignments,
                                    budget,
                                    &mut diagnosed,
                                )?
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some((batch, text)) = batch {
                ensure_state_writable(&state)?;
                let limit = remaining().min(timeout);
                let outcome = match &target {
                    Target::Codex(thread) => {
                        let scope_lock = order
                            .as_ref()
                            .and_then(|(_, scope_lock)| scope_lock.as_ref());
                        codex::queue(
                            thread,
                            &batch,
                            &text,
                            limit,
                            scope_lock,
                            target_lock.as_ref().expect("Codex claim holds target lock"),
                        )?
                    }
                    Target::OpenCode(target) => target.deliver(&text, limit),
                };
                // A full target pauses new claims. This batch already has
                // unknown receipts and remains available for explicit resolution.
                if !matches!(outcome, Outcome::Uncertain | Outcome::TargetFull) {
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
            } else if was_first || (recovery.is_empty() && scan.in_progress()) {
                // Advance an idle recovery scan promptly. A transport attempt
                // that reaches the retry path still waits below.
                continue;
            }
            drop(target_lock);
            drop(order);
            std::thread::sleep(remaining().min(Duration::from_millis(500)));
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
    TargetFull = 24,
}

fn claim_batch(
    inbox: &mut Inbox,
    scope: Option<&str>,
    assignments: &ScopeAssignments,
    budget: usize,
    diagnosed: &mut HashSet<u64>,
) -> Result<Option<(Vec<Value>, String)>> {
    let mut messages = Vec::new();
    let mut text = String::from(PREFIX);
    let dispatches = inbox.dir.join("dispatches");
    for m in &mut inbox.messages {
        if !m.claimable()
            || scope.is_some_and(|s| s != m.worker.scope())
        {
            continue;
        }
        if !assignments.permits(&dispatches, &m.worker) {
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

struct AssignmentProof {
    stamps: Option<GenerationStamps>,
    identity: Option<(String, u64)>,
    damaged: bool,
    boundary: MessageBoundary,
}

struct ScopeAssignments {
    directory: Option<Stamp>,
    proofs: HashMap<String, AssignmentProof>,
    restricted: bool,
}

impl ScopeAssignments {
    fn namespace_current(&self, state: &Path) -> Result<bool> {
        Ok(Stamp::at(&state.join("dispatches"))? == self.directory)
    }

    // Called after taking transport and scope locks and before taking the
    // inbox lock. Check the whole proof, including records with no messages.
    fn current_before_claim(&self, state: &Path, expired: impl Fn() -> bool) -> Result<bool> {
        let dir = state.join("dispatches");
        if !self.namespace_current(state)? {
            return Ok(false);
        }
        for (id, proof) in &self.proofs {
            if expired() || GenerationStamps::at(&dir, id).ok() != proof.stamps {
                return Ok(false);
            }
        }
        self.namespace_current(state)
    }

    fn permits(&self, dir: &Path, worker: &Identity) -> bool {
        if let Some(proof) = self.proofs.get(&worker.dispatch) {
            if worker.refusal(&proof.boundary).is_some()
                || proof.damaged
                || !proof
                    .stamps
                    .is_some_and(|stamps| stamps.current(dir, &worker.dispatch))
            {
                return false;
            }
            return !self.restricted
                || proof.identity.as_ref().is_some_and(|(attempt, round)| {
                    attempt == &worker.attempt && *round == worker.round
                });
        }
        !self.restricted
    }
}

// A truncated reservation leaves the global relay without trustworthy
// ownership. Scoped relays can still use independently verified assignments.
fn scope_assignments(
    state: &Path,
    scope: Option<&str>,
    previous: Option<ScopeAssignments>,
    expired: impl Fn() -> bool,
) -> Result<Option<ScopeAssignments>> {
    let dir = state.join("dispatches");
    let directory = Stamp::at(&dir)?;
    let mut old = previous.unwrap_or_else(|| ScopeAssignments {
        directory: None,
        proofs: HashMap::new(),
        restricted: false,
    });
    // The directory stamp only tracks the namespace. Recheck each pair's
    // stamps before reusing its parsed metadata, including damaged entries.
    let ids = if old.directory == directory {
        old.proofs.keys().cloned().collect()
    } else {
        crate::workflow::assignment::ids(&dir)?
    };
    let mut result = ScopeAssignments {
        directory,
        proofs: HashMap::new(),
        restricted: false,
    };
    for id in ids {
        if expired() {
            return Ok(None);
        }
        let stamps = GenerationStamps::at(&dir, &id).ok();
        let cached = old
            .proofs
            .remove(&id)
            .filter(|proof| stamps.is_some() && proof.stamps == stamps);
        let proof = match cached {
            Some(proof) => proof,
            None => {
                let generations = Generations::read(&dir, &id);
                let boundary = generations.message_boundary();
                for row in generations.rows() {
                    if !crate::workflow::assignment::tmux_history(row)
                        && !row["controllerOwner"].is_null()
                    {
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
                let unchanged = stamps.is_some_and(|stamps| stamps.current(&dir, &id));
                let identity = generations.assigned().and_then(|row| {
                    let assigned = if row["scope"].is_null() {
                        Some(id.as_str())
                    } else {
                        row["scope"].as_str()
                    };
                    if scope.is_none() || scope != assigned || !row["controllerOwner"].is_null() {
                        return None;
                    }
                    row["attempt"]
                        .as_str()
                        .zip(row["round"].as_u64())
                        .filter(|(attempt, round)| {
                            crate::workflow::valid_identity(attempt, "a-") && *round > 0
                        })
                        .map(|(attempt, round)| (attempt.to_owned(), round))
                });
                AssignmentProof {
                    stamps: stamps.filter(|_| unchanged),
                    identity,
                    damaged: !unchanged || generations.damaged,
                    boundary,
                }
            }
        };
        result.restricted |= proof.damaged;
        result.proofs.insert(id, proof);
    }
    Ok(Some(result))
}

fn settle(inbox: &mut Inbox, batch: &[Value], received: bool) -> Result<()> {
    let dir = inbox.dir.join("dispatches");
    let mut boundaries = HashMap::new();
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
            let boundary = boundaries
                .entry(m.worker.dispatch.clone())
                .or_insert_with(|| message_boundary(&dir, &m.worker.dispatch));
            if m.worker.refusal(boundary).is_some() {
                continue;
            }
            m.advance_delivery(if received {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_claim_leaves_retired_attempt_receipts_untouched() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-relay-history-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let dispatch = "d-00000031";
            let previous = "a-000000000031";
            let current = "a-000000000032";
            let launch = "1".repeat(32);
            let dir = state.join("dispatches");
            fs::create_dir_all(&dir)?;
            let assignment = json!({"dispatch":dispatch,"round":1,"attempt":current,
                "host":"codex","role":"fixture","scope":dispatch,"backend":"herdr",
                "audit":[{"operation":"handoff","dispatch":dispatch,"fromBackend":"tmux",
                          "fromRound":1,"fromAttempt":previous,"round":1,
                          "attempt":current,"toAttempt":current,"target":{"backend":"herdr"},
                          "phase":"complete","result":"accepted","oldWriterStopped":true,
                          "priorCreate":{"dispatch":dispatch,"round":1,"attempt":previous,
                                         "result":"accepted","receipt":format!("/fixture/{launch}.json")},
                          "close":{"dispatch":dispatch,"round":1,"attempt":previous,"result":"completed",
                                   "resourceCleanup":"confirmed","closeOperationId":format!("close:{previous}")},
                          "writerStopEvidence":{"dispatch":dispatch,"round":"1","attempt":previous,
                                                "launchId":launch,"writerTreeStopped":true}}]});
            let previous_message = json!({"dispatch":dispatch,"round":1,"attempt":previous,
                "role":"fixture","id":1,"messageId":"m-legacy","received":false,"report":"old"});
            let current_message = json!({"dispatch":dispatch,"round":1,"attempt":current,
                "role":"fixture","scope":dispatch,"id":2,"messageId":"m-current",
                "state":"unsent","statusVersion":1,"report":"new"});
            fs::write(
                dir.join(format!("{dispatch}.json")),
                serde_json::to_vec(&assignment)?,
            )?;
            let ledger = state.join("messages.json");
            fs::write(
                &ledger,
                serde_json::to_vec(&[&previous_message, &current_message])?,
            )?;

            let assignments =
                scope_assignments(&state, None, None, || false)?.context("missing proof")?;
            let mut inbox = Inbox::open(&state)?;
            let (batch, _) =
                claim_batch(&mut inbox, None, &assignments, 16_384, &mut HashSet::new())?
                    .context("missing current claim")?;
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0]["messageId"], "m-current");
            settle(&mut inbox, &batch, true)?;
            let saved: Vec<Value> = serde_json::from_slice(&fs::read(ledger)?)?;
            assert_eq!(saved[0], previous_message);
            assert_eq!(saved[1]["state"], "received");
            Ok(())
        })();
        let _ = fs::remove_dir_all(state);
        result
    }

    #[test]
    fn startup_relay_rechecks_legacy_owner_proof_under_transport_lock() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-relay-proof-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let dir = state.join("dispatches");
            fs::create_dir_all(&dir)?;
            let scope = "ours";
            let target = json!({"dispatch":"d-aabbccdd","scope":scope,"role":"fixture",
                "round":1,"attempt":"a-123456789abc"});
            let worker: Identity = serde_json::from_value(target.clone())?;
            let mut history = json!({"dispatch":"d-aabbccde","scope":scope,"role":"fixture",
                "round":1,"attempt":"a-123456789abd"});
            let path = dir.join("d-aabbccde.json");
            fs::write(dir.join("d-aabbccdd.json"), serde_json::to_vec(&target)?)?;
            fs::write(&path, serde_json::to_vec(&history)?)?;
            let proof = scope_assignments(&state, Some(scope), None, || false)?
                .context("missing original scope proof")?;

            // The older marker changes on a different assignment while the
            // relay's unlocked proof is in flight, using its existing locks.
            let writer = transport_lock(&state, Some(scope))?.context("missing writer lock")?;
            history["controllerOwner"] = json!({"owner":"historical fixture"});
            fs::write(&path, serde_json::to_vec(&history)?)?;
            drop(writer);

            let delivery = transport_lock(&state, Some(scope))?.context("missing relay lock")?;
            assert!(proof.namespace_current(&state)?);
            assert!(proof.permits(&dir, &worker));
            assert!(!proof.current_before_claim(&state, || false)?);
            assert!(!state.join("messages.lock").exists());
            drop(delivery);
            assert!(scope_assignments(&state, Some(scope), None, || false).is_err());
            let neighbor = scope_assignments(&state, Some("neighbor"), None, || false)?
                .context("missing neighboring scope proof")?;
            assert!(neighbor.current_before_claim(&state, || false)?);
            Ok(())
        })();
        let _ = fs::remove_dir_all(state);
        result
    }
}
