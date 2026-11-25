//! Linux backend boundary for the shared dispatch / reconciliation layers.
mod group;
mod runtime;
mod store;
mod user_service;
mod wire;

use crate::workflow::ensure_state_writable;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use store::{publish, Store};

const TIMEOUT: Duration = Duration::from_secs(6);
const TICK: Duration = Duration::from_millis(20);

fn fresh() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().with_context(|| format!("missing {key}"))
}

pub(crate) fn require_delegation() -> Result<()> {
    if std::env::var_os("WFC_BUNDLED_CGROUP_ROOT").is_some() {
        group::root().context(
            "bundled dispatch requires WFC_BUNDLED_CGROUP_ROOT to point to a writable, delegated cgroup v2 subtree with cgroup.kill",
        )?;
    } else {
        user_service::bus()?;
    }
    Ok(())
}

fn publisher_identity() -> Result<Value> {
    store::identity(std::process::id())
}

fn native_state(selected: Option<&Path>, inherited: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    if let Some(state) = selected {
        ensure!(
            state.is_absolute(),
            "bundled backend requires an absolute native state path"
        );
        return Ok(state.to_owned());
    }
    let state = inherited
        .filter(|path| !path.is_empty())
        .context("set WFC_STATE")?;
    Ok(std::path::absolute(state)?)
}

fn unit_dir(state: &Path, name: &str) -> Result<PathBuf> {
    ensure!(store::valid_name(name), "invalid unit");
    fs::create_dir_all(state)?;
    let backend = fs::canonicalize(state)?.join("bundled");
    store::private_dir(&backend)?;
    let unit = backend.join(name);
    store::private_dir(&unit)?;
    Ok(unit)
}

fn attached_store(state: &Path, spec: &Value) -> Result<(Store, File)> {
    let ticket = &spec["ticket"];
    let (unit, guard) = unit_lock_file(state, field(ticket, "unit")?)?;
    guard.lock()?;
    Ok((current_store(&unit, ticket)?, guard))
}

fn try_attached_store(state: &Path, spec: &Value) -> Result<Option<(Store, File)>> {
    let ticket = &spec["ticket"];
    let (unit, guard) = unit_lock_file(state, field(ticket, "unit")?)?;
    match guard.try_lock() {
        Ok(()) => Ok(Some((current_store(&unit, ticket)?, guard))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn current_store(unit: &Path, ticket: &Value) -> Result<Store> {
    let (_, store) = store::current(unit)?.context("missing current execution")?;
    ensure!(
        store.ticket == *ticket && store::ordered(unit, &store)?,
        "stale create"
    );
    Ok(store)
}

fn create_lock_file(unit: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(unit.join("lock"))?)
}

fn lock_for_write(state: &Path, file: File) -> Result<File> {
    file.lock()?;
    // The Python and native state can change while waiting for this lock.
    // Recheck with the original WFC_STATE still in the process environment.
    ensure_state_writable(state)?;
    Ok(file)
}

pub(crate) fn authorize_create<G, Check: FnOnce() -> Result<()>>(
    mut lock: impl FnMut() -> Result<File>,
    delegated: bool,
    authorize_ready: &impl Fn() -> Result<Option<(G, Check)>>,
) -> Result<(File, Option<(G, Check)>)> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let guard = lock()?;
        // Managed startup authorizes its signed decision after preparation.
        if !delegated {
            return Ok((guard, None));
        }
        if let Some(authorization) = authorize_ready()? {
            return Ok((guard, Some(authorization)));
        }
        // Input can take the unit lock and then check the Inbox. Release
        // the unit before retrying a temporarily busy publication lock.
        drop(guard);
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "cannot lock the delegated launch publication"
        );
        thread::sleep(remaining.min(TICK));
    }
}

pub(crate) fn create<G, Check: FnOnce() -> Result<()>>(
    state: &Path,
    req: &Value,
    authorize_ready: impl Fn() -> Result<Option<(G, Check)>>,
) -> Result<Value> {
    let unit = field(req, "unit")?;
    let attempt = field(req, "attempt")?;
    ensure!(store::valid_name(attempt), "invalid attempt");
    ensure!(
        req["argv"]
            .as_array()
            .is_some_and(|a| !a.is_empty() && a.iter().all(|v| v.is_string())),
        "invalid argv"
    );
    ensure!(
        Path::new(field(req, "cwd")?).is_absolute() && Path::new(field(req, "cwd")?).is_dir(),
        "invalid cwd"
    );
    if let Some(env) = req.get("env") {
        let env = env.as_object().context("env must be a string map")?;
        for (name, value) in env {
            ensure!(
                !name.is_empty()
                    && !name.contains(['=', '\0'])
                    && value.as_str().is_some_and(|s| !s.contains('\0')),
                "invalid worker environment"
            );
            ensure!(
                !matches!(
                    name.to_ascii_uppercase().as_str(),
                    "WFC_CONTROLLER_TOKEN" | "WFC_OWNER_TOKEN"
                ),
                "controller credential"
            );
        }
    }
    ensure_state_writable(state)?;
    let unit_path = unit_dir(state, unit)?;
    let delegated = std::env::var_os("WFC_BUNDLED_CGROUP_ROOT").is_some();
    let (guard, authorization) = authorize_create(
        || lock_for_write(state, create_lock_file(&unit_path)?),
        delegated,
        &authorize_ready,
    )?;
    let current_path = unit_path.join("current.json");
    let current = match store::current(&unit_path) {
        Ok(current) => current,
        Err(_) => return Ok(unknown_cleanup()),
    };
    if let Some((record, prior)) = &current {
        if !store::ordered(&unit_path, prior).unwrap_or(false) {
            return Ok(unknown_cleanup());
        }
        if req["previous"] != prior.ticket || prior.ticket["attempt"] == attempt {
            return Ok(json!({"result":"refused","diagnostic":"current create fence"}));
        }
        if !prior.replaceable(record) {
            return Ok(unknown_cleanup());
        }
    } else {
        ensure!(req["previous"].is_null(), "missing previous create");
    }
    let root = if delegated {
        match group::root() {
            Ok(root) => Some(root),
            Err(error) => {
                return Ok(json!({"result":"definitely-not-sent","diagnostic":error.to_string()}))
            }
        }
    } else {
        None
    };
    let creator = publisher_identity()?;
    let expires = unix_ms()? + (TIMEOUT + TIMEOUT).as_millis() as u64;
    let user_start = if root.is_none() {
        Some(user_service::context(state, &creator, expires)?)
    } else {
        None
    };
    let generation = fresh();
    let token = format!("{}{}", fresh(), fresh());
    let mut ticket = json!({"version":2,"unit":unit,"attempt":attempt,"generation":generation,"nonce":fresh(),"capabilityHash":store::digest(token.as_bytes())});
    if user_start.is_some() {
        ticket["startupProtocol"] = json!(1);
    }
    let run = unit_path.join(&generation);
    store::private_dir(&run)?;
    publish(&run.join("key.json"), &json!(token))?;
    let spec = json!({"ticket":ticket,"request":req,"userService":user_start});
    let explicit = root
        .map(|root| explicit_guardian(state, &run, &spec, &token, root))
        .transpose()?;
    let store = Store::open(&unit_path, &ticket)?;
    let index =
        json!({"ticket":ticket,"applied":false,"creator":creator,"pending_until_ms":expires});
    // A trusted recovery copy exists before retiring the prior generation.
    // Retire it before installing any successor that could authorize a launch.
    store::publish_current(&run.join("index.json"), index.clone(), &store)?;
    if let Some((_, prior)) = current {
        prior.save_signed("superseded", json!({"generation":generation}))?;
    }
    if let Err(error) = store::publish_current(&current_path, index, &store) {
        store.save_signed(
            "unstarted",
            json!({"reason":"index publication failed","group_cleanup":"not-created"}),
        )?;
        return Ok(
            json!({"result":"definitely-not-sent","ticket":ticket,"diagnostic":error.to_string()}),
        );
    }
    if let Some(context) = &user_start {
        let launcher = user_service::start(&store);
        drop(guard);
        return user_service::await_ready(state, &store, &spec, context, launcher, authorize_ready);
    }
    let (publication, check_owner) =
        authorization.context("missing delegated launch authorization")?;
    if let Err(answer) = check_delegated_launch(state, &store, check_owner) {
        return Ok(answer);
    }
    let mut child = match explicit.context("missing explicit guardian")?.spawn() {
        Ok(c) => c,
        Err(_) => {
            store.save_signed(
                "unstarted",
                json!({"reason":"guardian did not start","group_cleanup":"not-created"}),
            )?;
            return Ok(json!({"result":"definitely-not-sent","ticket":ticket}));
        }
    };
    drop(publication);
    drop(guard);
    let end = Instant::now() + TIMEOUT + TIMEOUT;
    loop {
        if let Ok(identity) = store.signed("ready") {
            return Ok(json!({"result":"accepted","ticket":ticket,"identity":identity}));
        }
        if let Ok(info) = store.signed("unstarted") {
            return Ok(json!({"result":"definitely-not-sent","ticket":ticket,"unstarted":info}));
        }
        if child.try_wait()?.is_some() || Instant::now() >= end {
            return Ok(json!({"result":"uncertain","ticket":ticket}));
        }
        thread::sleep(TICK);
    }
}

fn check_delegated_launch(
    state: &Path,
    store: &Store,
    authorize: impl FnOnce() -> Result<()>,
) -> std::result::Result<(), Value> {
    ensure_state_writable(state)
        .and_then(|()| authorize())
        .map_err(|error| {
            // The new index already names this generation. Record that it
            // never launched, unless the state write fence prevents evidence.
            let unstarted = json!({"reason":"delegated launch authorization failed",
                "group_cleanup":"not-created"});
            let recorded = ensure_state_writable(state)
                .and_then(|()| store.save_signed("unstarted", unstarted))
                .is_ok();
            let result = if recorded {
                "definitely-not-sent"
            } else {
                "uncertain"
            };
            json!({"result":result,"ticket":store.ticket,"diagnostic":format!("{error:#}")})
        })
}

fn explicit_guardian(
    state: &Path,
    run: &Path,
    spec: &Value,
    token: &str,
    root: PathBuf,
) -> Result<Command> {
    let mut cmd = store::stage_cmd("--bundled-guardian", run, spec, token)?;
    cmd.arg(state).env("WFC_BUNDLED_CGROUP_ROOT", root);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error().into())
            } else {
                Ok(())
            }
        });
    }
    Ok(cmd)
}

fn service_changed() -> Value {
    json!({"result":"uncertain","delivery":"definitely-not-sent","cleanup":"unknown","category":"bundled_restart","reconcile":true})
}

// The flag records whether the authenticated request frame may have left the client.
fn rpc(store: &Store, ready: &Value, req: &Value) -> (Value, bool) {
    rpc_before_send(store, ready, req, || true)
}

fn rpc_before_send(
    store: &Store,
    ready: &Value,
    req: &Value,
    before_send: impl FnOnce() -> bool,
) -> (Value, bool) {
    if store::process_state(&ready["service"]) != "alive" {
        return (service_changed(), false);
    }
    let mut sent = false;
    let outcome = (|| -> Result<Value> {
        let port = u16::try_from(ready["endpoint"].as_u64().context("missing port")?)?;
        let end = Instant::now() + TIMEOUT;
        let mut stream = wire::Wire::new(TcpStream::connect_timeout(
            &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
            TIMEOUT,
        )?);
        let client = fresh();
        stream.write(&json!({"challenge":client}), end)?;
        let response = stream.read(256, end)?;
        let challenge = field(&response, "challenge")?;
        store::verify(
            &store.token,
            format!("server:{client}:{challenge}").as_bytes(),
            field(&response, "proof")?,
        )?;
        stream.write(&json!({"proof":store::sign(&store.token,format!("client:{client}:{challenge}").as_bytes())}),end)?;
        let mut frame = req.clone();
        let id = fresh();
        frame["id"] = json!(id);
        let bytes = wire::encode(&frame)?;
        if !before_send() {
            return Ok(json!({"result":"definitely-not-sent"}));
        }
        sent = true;
        stream.write_bytes(&bytes, end)?;
        let reply = stream.read(wire::LIMIT, end)?;
        ensure!(
            reply["ticket"] == store.ticket && reply["id"] == id,
            "response identity mismatch"
        );
        ensure!(
            matches!(
                reply["result"].as_str(),
                Some("accepted" | "refused" | "uncertain")
            ),
            "invalid response"
        );
        Ok(reply)
    })();
    let reply = outcome.unwrap_or_else(|_| {
        if !sent && store::process_state(&ready["service"]) != "alive" {
            return service_changed();
        }
        json!({"result":if sent {"uncertain"} else {"definitely-not-sent"}, "cleanup":"unknown"})
    });
    (reply, sent)
}

fn unknown_cleanup() -> Value {
    json!({"result":"uncertain","cleanup":"unknown"})
}

// Observations open the existing unit without recreating lost directories or locks.
fn open_unit(state: &Path, name: &str) -> Result<(PathBuf, File)> {
    let (unit, guard) = unit_lock_file(state, name)?;
    guard.lock_shared()?;
    Ok((unit, guard))
}

fn open_write_unit(state: &Path, name: &str) -> Result<(PathBuf, File)> {
    let (unit, guard) = unit_lock_file(state, name)?;
    Ok((unit, lock_for_write(state, guard)?))
}

fn resume_source(unit: &Path, ticket: &Value) -> Result<(Value, Store, Value)> {
    let (current, store) = store::current(unit)?.context("missing original bundled execution")?;
    ensure!(
        &store.ticket == ticket && ticket["version"] == 2 && store::ordered(unit, &store)?,
        "bundled execution changed before resume"
    );
    let proof = store.resume_proof()?;
    Ok((current, store, proof))
}

pub(crate) fn inspect_resume_cleanup(state: &Path, ticket: &Value) -> Result<Value> {
    let (unit, _guard) = open_unit(state, field(ticket, "unit")?)?;
    let (_, _, proof) = resume_source(&unit, ticket)?;
    Ok(proof)
}

pub(crate) fn apply_resume_cleanup(state: &Path, ticket: &Value, expected: &Value) -> Result<()> {
    let (unit, _guard) = open_write_unit(state, field(ticket, "unit")?)?;
    let (current, store, proof) = resume_source(&unit, ticket)?;
    ensure!(
        &proof == expected,
        "original bundled cleanup could not be verified"
    );
    let applied = store.reconcile(&unit.join("current.json"), current, proof)?;
    ensure!(
        applied["result"] == "completed",
        "original bundled cleanup is not settled"
    );
    Ok(())
}

pub(crate) fn confirm_resume_start(state: &Path, ticket: &Value, identity: &Value) -> Result<bool> {
    let (unit, _guard) = open_unit(state, field(ticket, "unit")?)?;
    let (_, store) = store::current(&unit)?.context("missing bundled replacement")?;
    Ok(identity.is_object()
        && &store.ticket == ticket
        && store::ordered(&unit, &store)?
        && store.signed("ready")? == *identity)
}

fn operation_unit(state: &Path, name: &str, op: &str) -> Result<(PathBuf, File)> {
    if matches!(op, "close" | "input" | "resize" | "reconcile") {
        open_write_unit(state, name)
    } else {
        open_unit(state, name)
    }
}

fn unit_lock_file(state: &Path, name: &str) -> Result<(PathBuf, File)> {
    ensure!(store::valid_name(name), "invalid unit");
    let backend = state.join("bundled");
    store::check_private_dir(&backend)?;
    let unit = backend.join(name);
    store::check_private_dir(&unit)?;
    let guard = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(unit.join("lock"))?;
    ensure!(guard.metadata()?.is_file(), "invalid unit lock");
    Ok((unit, guard))
}

/// The caller holds the workflow's scope and attempt locks. This lock also
/// excludes the existing private bundled caller while observing and writing.
pub(crate) struct InputSession {
    store: Store,
    ready: Value,
    _guard: File,
}

impl InputSession {
    pub(crate) fn open(state: &Path, ticket: &Value, identity: &Value) -> Result<Self> {
        let (unit, guard) = open_write_unit(state, field(ticket, "unit")?)?;
        let (_, store) = store::current(&unit)?.context("missing current bundled execution")?;
        ensure!(
            ticket["version"] == 2 && store.ticket == *ticket && store::ordered(&unit, &store)?,
            "bundled input requires the registered current execution"
        );
        ensure!(
            fs::symlink_metadata(store.run.join("cleanup.json"))
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
            "bundled execution already has cleanup evidence"
        );
        let ready = store.signed("ready")?;
        ensure!(
            identity.is_object()
                && ready == *identity
                && store::process_state(&ready["worker"]) == "alive",
            "bundled execution identity is not a verified live worker"
        );
        Ok(Self {
            store,
            ready,
            _guard: guard,
        })
    }

    pub(crate) fn read(&self) -> Result<Value> {
        let (answer, _) = rpc(
            &self.store,
            &self.ready,
            &json!({"op":"read","ticket":self.store.ticket,"styled":true}),
        );
        let data = &answer["data"];
        ensure!(
            answer["result"] == "accepted"
                && data["exit"].is_null()
                && data["read_error"].is_null()
                && data["eof"] == false
                && data["screen"] == "Primary",
            "bundled worker does not have a verified live primary viewport"
        );
        Ok(answer)
    }

    /// The second field says whether the operation frame may have left the client.
    pub(crate) fn input(
        &self,
        prior: u64,
        text: &str,
        check_write: impl FnOnce() -> Result<()>,
    ) -> (Result<Value>, bool) {
        let Some(seq) = prior.checked_add(1) else {
            return (
                Ok(
                    json!({"result":"definitely-not-sent","diagnostic":"bundled input sequence exhausted"}),
                ),
                false,
            );
        };
        if store::process_state(&self.ready["worker"]) != "alive" {
            return (
                Ok(
                    json!({"result":"definitely-not-sent","diagnostic":"bundled worker changed before input"}),
                ),
                false,
            );
        }
        let mut authorization = Ok(());
        let (reply, sent) = rpc_before_send(
            &self.store,
            &self.ready,
            &json!({"op":"input","ticket":self.store.ticket,"seq":seq,"text":text}),
            || {
                authorization = check_write();
                authorization.is_ok()
            },
        );
        if let Err(error) = authorization {
            return (Err(error), sent);
        }
        let answer = if !sent {
            json!({"result":"definitely-not-sent","diagnostic":"bundled input frame was not sent"})
        } else if reply["result"] == "accepted"
            && reply["last_input"] == seq
            && reply["data"]["written"] == true
        {
            reply
        } else if reply["result"] == "refused" {
            json!({"result":"refused","diagnostic":"bundled runtime refused the input sequence"})
        } else {
            json!({"result":"uncertain","diagnostic":"bundled runtime did not confirm the input frame"})
        };
        (Ok(answer), sent)
    }
}

/// The flag authorizes withdrawing this call's intent for an exact, unsent close.
pub(crate) fn assignment_operation(state: &Path, ticket: &Value, op: &str) -> (Value, bool) {
    let mut withdraw = false;
    let outcome = (|| -> Result<Value> {
        let name = field(ticket, "unit")?;
        let (unit, _guard) = if op == "close" {
            let (unit, guard) = unit_lock_file(state, name)?;
            guard.lock()?;
            (unit, guard)
        } else {
            operation_unit(state, name, op)?
        };
        let current = store::current(&unit).ok().flatten();
        let ordered = current
            .as_ref()
            .is_some_and(|(_, selected)| store::ordered(&unit, selected).unwrap_or(false));
        if ordered
            && current
                .as_ref()
                .is_some_and(|(_, selected)| selected.ticket != *ticket)
        {
            return Ok(json!({"result":"refused","diagnostic":"stale execution identity"}));
        }
        let target = Store::open(&unit, ticket)?;
        let legacy = ticket.get("version").is_none();
        let exact =
            ordered && !legacy && current.as_ref().is_some_and(|(_, s)| s.ticket == *ticket);
        if let Ok(proof) = target.proof() {
            if ordered && (!legacy || target.current_proof().is_ok()) {
                return Ok(terminal_reply(&target, proof, op));
            }
            return Ok(
                json!({"result":"uncertain","scope":"execution","cleanup":"unknown","ticket":ticket,
                "execution_cleanup":"confirmed","execution_proof":proof}),
            );
        }
        if op == "close" {
            // Read the authenticated binding under the unit lock even when
            // its legacy write fence prevents sending any part of the request.
            if let Err(error) = ensure_state_writable(state) {
                let answer = json!({"result":"uncertain","cleanup":"unknown","diagnostic":format!("{error:#}")});
                withdraw = unsent_close(op, exact, false, &answer);
                return Ok(answer);
            }
        }
        if op == "cleanup-receipt" || current.is_none() {
            return Ok(unknown_cleanup());
        }
        if op == "inspect" && exact && target.signed("ready").is_err() {
            if let Ok(prepared) = target.signed("prepared") {
                let phase = match target.startup_decision(&prepared)? {
                    None => "prepared",
                    Some(store::StartupDecision::Aborted) => "aborted",
                    Some(store::StartupDecision::Ready) => return Ok(unknown_cleanup()),
                };
                return Ok(json!({"result":"uncertain","startup":phase,
                    "startupIdentity":prepared,"cleanup":"unknown"}));
            }
            if let Ok(info) = target.signed("unstarted") {
                return Ok(json!({"result":"definitely-not-sent","startup":"unstarted",
                    "diagnostic":info["reason"]}));
            }
        }
        let mut sent = false;
        let answer = live_operation(
            &target,
            &json!({"op":op,"ticket":ticket}),
            op,
            !ordered || legacy,
            &mut sent,
        );
        withdraw = unsent_close(op, exact, sent, &answer);
        Ok(answer)
    })();
    (outcome.unwrap_or_else(|_| unknown_cleanup()), withdraw)
}

fn unsent_close(op: &str, exact: bool, sent: bool, answer: &Value) -> bool {
    op == "close"
        && exact
        && !sent
        && answer["result"] != "completed"
        && answer["execution_cleanup"] != "confirmed"
}

fn terminal_reply(store: &Store, proof: Value, op: &str) -> Value {
    if matches!(op, "input" | "resize") {
        json!({"result":"definitely-not-sent", "delivery":"definitely-not-sent",
               "cleanup":"confirmed", "proof":proof, "ticket":store.ticket})
    } else {
        json!({"result":"completed", "proof":proof, "ticket":store.ticket})
    }
}

// A directed operation addresses the authenticated runtime of this exact ticket.
fn live_operation(store: &Store, req: &Value, op: &str, targeted: bool, sent: &mut bool) -> Value {
    let observing = matches!(op, "inspect" | "close" | "read");
    if targeted && !observing && store.run.join("cleanup.json").try_exists().unwrap_or(true) {
        return unknown_cleanup();
    }
    let Ok(ready) = store.signed("ready") else {
        return unknown_cleanup();
    };
    let (mut answer, attempted) = rpc(store, &ready, req);
    *sent = attempted;
    let accepted = answer["result"] == "accepted";
    if op == "close" && (accepted || (targeted && attempted && answer["result"] == "uncertain")) {
        let end = Instant::now() + TIMEOUT;
        loop {
            if let Ok(proof) = store.proof() {
                if targeted {
                    answer["scope"] = json!("execution");
                    answer["ticket"] = store.ticket.clone();
                    answer["cleanup"] = json!("unknown");
                    answer["execution_cleanup"] = json!("confirmed");
                    answer["execution_proof"] = proof;
                    return answer;
                }
                return terminal_reply(store, proof, op);
            }
            // A lost reply gets one read-only check. Only an acknowledged
            // close waits for the guardian to finish publishing its receipt.
            if !accepted || Instant::now() >= end {
                return unknown_cleanup();
            }
            thread::sleep(TICK);
        }
    }
    if accepted {
        if targeted {
            answer["scope"] = json!("execution");
            answer["cleanup"] = json!("unknown");
        }
        if op == "inspect" {
            answer["identity"] = ready;
        }
    } else if answer["result"] != "refused" {
        // A terminal receipt can settle an observation or a pre-send failure.
        // It says nothing about bytes sent before a reply was lost.
        let unsent = answer["delivery"] == "definitely-not-sent"
            || answer["result"] == "definitely-not-sent";
        if !targeted && (observing || unsent) {
            if let Ok(proof) = store.current_proof() {
                return terminal_reply(store, proof, op);
            }
        }
        if observing && answer["reconcile"] != true {
            return unknown_cleanup();
        }
    }
    answer
}

fn unix_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub(crate) fn lookup_assignment(state: &Path, name: &str, attempt: &Value) -> Result<Value> {
    let (unit, _guard) = open_unit(state, name)?;
    lookup(&unit, attempt)
}

fn lookup(unit: &Path, attempt: &Value) -> Result<Value> {
    let (current, store) = match store::current(unit) {
        Ok(Some(c)) => c,
        Ok(None) => bail!("unknown unit"),
        Err(_) => return Ok(unknown_cleanup()),
    };
    if !store::ordered(unit, &store).unwrap_or(false) {
        return Ok(unknown_cleanup());
    }
    ensure!(current["ticket"]["attempt"] == *attempt, "wrong attempt");
    let phase = if store.current_proof().is_ok() {
        "cleaned"
    } else if store.run.join("cleanup.json").try_exists()? {
        return Ok(unknown_cleanup());
    } else if store.signed("unstarted").is_ok() {
        "unstarted"
    } else if store.signed("ready").is_ok() {
        "ready"
    } else {
        "pending"
    };
    Ok(
        json!({"result":"accepted","ticket":current["ticket"],"phase":phase,
        "creator":current["creator"],"creator_state":store::process_state(&current["creator"]),
        "pending_until_ms":current["pending_until_ms"],"pending_expired":phase=="pending" && unix_ms()? >= current["pending_until_ms"].as_u64().context("missing pending deadline")?}),
    )
}

fn operate(state: &Path, req: &Value) -> Result<Value> {
    let op = field(req, "op")?;
    if op == "create" {
        return create(state, req, || Ok(Some(((), || Ok(())))));
    }
    if op == "lookup" {
        let (unit, _guard) = open_unit(state, field(req, "unit")?)?;
        return lookup(&unit, &req["attempt"]);
    }
    let ticket = &req["ticket"];
    let (unit, _guard) = operation_unit(state, field(ticket, "unit")?, op)?;
    let (current, store) = match store::current(&unit) {
        Ok(Some(c)) => c,
        Ok(None) => bail!("unknown unit"),
        Err(_) => return Ok(unknown_cleanup()),
    };
    ensure!(
        matches!(
            op,
            "inspect" | "close" | "input" | "read" | "resize" | "cleanup-receipt" | "reconcile"
        ),
        "unknown operation"
    );
    if !store::ordered(&unit, &store).unwrap_or(false) {
        if matches!(op, "inspect" | "read" | "close") {
            if let Ok(target) = Store::open(&unit, ticket) {
                return Ok(live_operation(&target, req, op, true, &mut false));
            }
        }
        return Ok(unknown_cleanup());
    }
    if current["ticket"] != *ticket {
        return Ok(json!({"result":"refused","diagnostic":"stale execution identity"}));
    }
    if let Ok(proof) = store.current_proof() {
        if op == "reconcile" {
            return Ok(store
                .reconcile(&unit.join("current.json"), current, proof)
                .unwrap_or_else(|_| unknown_cleanup()));
        }
        return Ok(terminal_reply(&store, proof, op));
    }
    if matches!(op, "cleanup-receipt" | "reconcile") {
        return Ok(unknown_cleanup());
    }
    Ok(live_operation(
        &store,
        req,
        op,
        ticket.get("version").is_none(),
        &mut false,
    ))
}

pub(crate) fn assignment_ready(state: &Path, unit: &str, row: &Value) -> Result<bool> {
    // Callback publication holds the Inbox lock. Input holds the unit lock
    // across its Inbox checks, so read the signed evidence without waiting.
    let (path, _guard) = unit_lock_file(state, unit)?;
    let Some((_, store)) = store::current(&path)? else {
        return Ok(false);
    };
    Ok(store.ticket["attempt"] == row["attempt"]
        && [&row["create"]["ticket"], &row["backendHandle"]]
            .into_iter()
            .all(|ticket| ticket.is_null() || ticket == &store.ticket)
        && store::ordered(&path, &store)?
        && store.signed("ready").is_ok()
        // Do not accept evidence from a generation replaced during the read.
        && store::current(&path)?
            .is_some_and(|(_, selected)| selected.ticket == store.ticket))
}

pub fn run(stage: &str, selected: Option<&Path>) -> Result<u8> {
    let managed = stage == user_service::STAGE;
    let mut bytes = Vec::new();
    // Internal specs also carry the generated ticket around the caller's frame.
    let limit = wire::LIMIT + if stage == "--bundled" { 0 } else { 4096 };
    io::stdin().take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "oversized request");
    let request: Value = serde_json::from_slice(&bytes)?;
    let spec = &request["spec"];
    let inherited = std::env::var_os("WFC_STATE");
    let selected = if managed {
        Some(Path::new(field(&spec["userService"], "state")?))
    } else {
        selected
    };
    let state = native_state(selected, inherited.as_deref())?;
    match stage {
        "--bundled-guardian" | "--bundled-runtime" | user_service::STAGE => {
            let (store, guard) = attached_store(&state, spec)?;
            let grant = json!([stage, spec]);
            store::verify(
                &store.token,
                &serde_json::to_vec(&grant)?,
                field(&request, "signature")?,
            )?;
            if managed {
                user_service::restore_legacy_context(&spec["userService"])?;
                ensure!(
                    fs::symlink_metadata(store.run.join("unstarted.json"))
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound),
                    "bundled user service grant was withdrawn"
                );
            }
            ensure_state_writable(&state)?;
            // Consume the signed grant once, including if the stage dies while starting.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(store.run.join(stage))?;
            // The next stage also acquires this lock before consuming its grant.
            drop(guard);
            if managed {
                user_service::guard(&state, spec, store)?;
            } else if stage == "--bundled-guardian" {
                runtime::guard(&state, spec, store, None)?;
            } else {
                return runtime::serve(spec, store);
            }
        }
        _ => println!(
            "{}",
            operate(&state, &request)
                .unwrap_or_else(|e| json!({"result":"refused","diagnostic":e.to_string()}))
        ),
    }
    Ok(0)
}

#[cfg(test)]
pub(crate) fn ticket_cleanup_fixture(
    state: &Path,
    unit: &str,
    attempt: &str,
    cleaned: bool,
) -> Result<(Value, PathBuf)> {
    let phase = if cleaned {
        ResumeFixture::Closed
    } else {
        ResumeFixture::Ready
    };
    let (ticket, cleanup, _) = resume_ticket_fixture(state, unit, attempt, phase, false)?;
    Ok((ticket, cleanup))
}

#[cfg(test)]
pub(crate) enum ResumeFixture {
    Ready,
    Closed,
    ReadyFault,
    StartupFault,
}

#[cfg(test)]
pub(crate) fn resume_ticket_fixture(
    state: &Path,
    unit: &str,
    attempt: &str,
    phase: ResumeFixture,
    managed: bool,
) -> Result<(Value, PathBuf, Value)> {
    let unit_path = unit_dir(state, unit)?;
    let _guard = lock_for_write(state, create_lock_file(&unit_path)?)?;
    let previous = store::current(&unit_path)?;
    let token = fresh();
    let generation = fresh();
    let mut ticket = json!({"version":2,"unit":unit,"attempt":attempt,"generation":generation,
        "nonce":fresh(),"capabilityHash":store::digest(token.as_bytes())});
    if managed {
        ticket["startupProtocol"] = json!(1);
    }
    let run = unit_path.join(generation);
    store::private_dir(&run)?;
    publish(&run.join("key.json"), &json!(token))?;
    let store = Store::open(&unit_path, &ticket)?;
    let process = store::identity(std::process::id())?;
    let index = json!({"ticket":ticket,"applied":false,"creator":process,"pending_until_ms":1});
    store::publish_current(&run.join("index.json"), index.clone(), &store)?;
    if let Some((current, prior)) = previous {
        ensure!(
            prior.replaceable(&current),
            "fixture requires applied cleanup"
        );
        prior.save_signed("superseded", json!({"generation":ticket["generation"]}))?;
    }
    store::publish_current(&unit_path.join("current.json"), index, &store)?;
    let ready = json!({"guardian":process,"service":process,"kernel":process,"worker":process,
        "endpoint":42,"kernel_generation":fresh(),
        "containment":{"kind":"cgroup-v2","path":"/fixture","device":1,"inode":2}});
    if matches!(phase, ResumeFixture::StartupFault) {
        let starting = json!({"guardian":ready["guardian"],"containment":ready["containment"]});
        store.save_signed("starting", starting.clone())?;
        store.save_signed(
            "cleanup",
            json!({"state":"faulted","proves":"execution-empty","identity":starting}),
        )?;
    } else if managed {
        store.save_signed("prepared", ready.clone())?;
        store.commit_ready(&ready, || Ok(()))?;
    } else {
        store.save_signed("ready", ready.clone())?;
    }
    let terminal = match phase {
        ResumeFixture::Closed => Some("closed"),
        ResumeFixture::ReadyFault => Some("faulted"),
        ResumeFixture::StartupFault | ResumeFixture::Ready => None,
    };
    if let Some(terminal) = terminal {
        store.save_signed(
            "cleanup",
            json!({"state":terminal,"proves":"execution-empty","identity":ready}),
        )?;
    }
    Ok((ticket, run.join("cleanup.json"), ready))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn delegated_launch_denial_preserves_ticket_and_respects_write_fence() -> Result<()> {
        let state = std::env::temp_dir().join(fresh());
        store::private_dir(&state)?;
        let result = (|| -> Result<()> {
            for blocked in [false, true] {
                let run = state.join(fresh());
                store::private_dir(&run)?;
                let store = Store {
                    run,
                    ticket: json!({"version":2,"unit":"resume-fixture","generation":fresh()}),
                    token: fresh(),
                };
                let fence = state.join("router.db-journal");
                if blocked {
                    fs::write(&fence, b"ordinary synthetic legacy fence")?;
                }
                let answer = check_delegated_launch(&state, &store, || {
                    assert!(!blocked, "write fence must run before owner authorization");
                    bail!("synthetic lease expired before launch")
                })
                .expect_err("denied delegated launch must not spawn");
                assert_eq!(answer["ticket"], store.ticket);
                if blocked {
                    assert_eq!(answer["result"], "uncertain");
                    assert!(!store.run.join("unstarted.json").try_exists()?);
                    assert_eq!(fs::read(&fence)?, b"ordinary synthetic legacy fence");
                } else {
                    assert_eq!(answer["result"], "definitely-not-sent");
                    assert_eq!(store.signed("unstarted")?["group_cleanup"], "not-created");
                }
            }
            Ok(())
        })();
        fs::remove_dir_all(&state)?;
        result
    }

    #[test]
    fn close_withdrawal_requires_verified_unsent_exact_execution() {
        let unknown = unknown_cleanup();
        assert!(unsent_close("close", true, false, &unknown));
        assert!(!unsent_close("close", true, true, &unknown));
        assert!(!unsent_close("close", false, false, &unknown));
        assert!(!unsent_close("cleanup-receipt", true, false, &unknown));
        assert!(!unsent_close(
            "close",
            true,
            false,
            &json!({"result":"completed"})
        ));
        assert!(!unsent_close(
            "close",
            true,
            false,
            &json!({"execution_cleanup":"confirmed"})
        ));
    }

    fn authenticate_client(
        socket: &mut wire::Wire<TcpStream>,
        token: &str,
        end: Instant,
    ) -> Result<()> {
        let hello = socket.read(256, end)?;
        let client = field(&hello, "challenge")?;
        let challenge = fresh();
        let proof = store::sign(token, format!("server:{client}:{challenge}").as_bytes());
        socket.write(&json!({"challenge":challenge,"proof":proof}), end)?;
        let auth = socket.read(256, end)?;
        store::verify(
            token,
            format!("client:{client}:{challenge}").as_bytes(),
            field(&auth, "proof")?,
        )
    }

    #[test]
    fn lost_directed_close_reply_confirms_only_the_execution() -> Result<()> {
        let store = Store {
            run: std::env::temp_dir().join(fresh()),
            ticket: json!({"unit":"probe","attempt":"close","generation":fresh(),"nonce":fresh()}),
            token: fresh(),
        };
        store::private_dir(&store.run)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let pid = store::identity(std::process::id())?;
        let ready = json!({"guardian":pid, "service":pid, "kernel":pid, "worker":pid,
            "endpoint":listener.local_addr()?.port(), "kernel_generation":fresh(),
            "containment":{"kind":"cgroup-v2","path":"/fixture","device":1,"inode":2}});
        let proof = json!({"state":"closed","proves":"execution-empty","identity":ready});
        store.save_signed("ready", ready)?;
        let answer = thread::scope(|scope| -> Result<Value> {
            let peer = scope.spawn(|| -> Result<()> {
                let mut socket = wire::Wire::new(listener.accept()?.0);
                let end = Instant::now() + TIMEOUT;
                authenticate_client(&mut socket, &store.token, end)?;
                let frame = socket.read(wire::LIMIT, end)?;
                assert_eq!(frame["op"], "close");
                assert_eq!(frame["ticket"], store.ticket);
                store.save_signed("cleanup", proof.clone())
            });
            let req = json!({"op":"close","ticket":store.ticket});
            let answer = live_operation(&store, &req, "close", true, &mut false);
            peer.join().unwrap()?;
            Ok(answer)
        })?;
        listener.set_nonblocking(true)?;
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(answer["result"], "uncertain");
        assert_eq!(answer["ticket"], store.ticket);
        assert_eq!(answer["scope"], "execution");
        assert_eq!(answer["execution_cleanup"], "confirmed");
        assert_eq!(answer["execution_proof"], proof);
        assert_eq!(answer["cleanup"], "unknown");
        fs::remove_dir_all(store.run)?;
        Ok(())
    }

    #[test]
    fn input_failure_tracks_whether_the_request_left_the_client() {
        let store = Store {
            run: PathBuf::new(),
            ticket: json!({"unit":"probe","attempt":"a","generation":fresh(),"nonce":fresh()}),
            token: fresh(),
        };
        for fault in ["wrong-server", "lost-reply", "wrong-id"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let ready = json!({"service":store::identity(std::process::id()).unwrap(),"endpoint":listener.local_addr().unwrap().port()});
            thread::scope(|scope| {
                let token = &store.token;
                let server = scope.spawn(move || {
                    let mut socket = wire::Wire::new(listener.accept().unwrap().0);
                    let end = Instant::now()+TIMEOUT;
                    let key = if fault == "wrong-server" {"other"} else {token};
                    let auth = authenticate_client(&mut socket, key, end);
                    if fault == "wrong-server" {
                        assert!(auth.is_err());
                    } else {
                        auth.unwrap();
                        let frame = socket.read(wire::LIMIT,end).unwrap();
                        assert_eq!(frame["text"],"once");
                        if fault == "wrong-id" {
                            socket.write(&json!({"ticket":frame["ticket"],"result":"accepted","id":"wrong"}),end).unwrap();
                        }
                    }
                    drop(socket);
                    listener.set_nonblocking(true).unwrap();
                    assert!(listener.accept().is_err());
                });
                let (answer, attempted) = rpc(
                    &store,
                    &ready,
                    &json!({"op":"input","seq":1,"text":"once","ticket":store.ticket}),
                );
                assert_eq!(
                    answer["result"],
                    if fault == "wrong-server" {
                        "definitely-not-sent"
                    } else {
                        "uncertain"
                    }
                );
                assert_eq!(attempted, fault != "wrong-server");
                server.join().unwrap();
            });
        }
    }
}
