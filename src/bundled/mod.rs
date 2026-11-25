//! Linux backend boundary for the shared dispatch / reconciliation layers.
mod group;
mod runtime;
mod store;
mod wire;

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
    group::root().context(
        "bundled dispatch requires WFC_BUNDLED_CGROUP_ROOT to point to a writable, delegated cgroup v2 subtree with cgroup.kill",
    )?;
    Ok(())
}

fn unit_dir(name: &str) -> Result<PathBuf> {
    ensure!(store::valid_name(name), "invalid unit");
    let state = std::env::var_os("WFC_STATE")
        .filter(|s| !s.is_empty())
        .context("set WFC_STATE")?;
    fs::create_dir_all(&state)?;
    let backend = fs::canonicalize(state)?.join("bundled");
    store::private_dir(&backend)?;
    let unit = backend.join(name);
    store::private_dir(&unit)?;
    Ok(unit)
}

fn attached_store(spec: &Value) -> Result<Store> {
    let ticket = &spec["ticket"];
    let unit = unit_dir(field(ticket, "unit")?)?;
    let _guard = lock(&unit)?;
    let (_, store) = store::current(&unit)?.context("missing current execution")?;
    ensure!(
        store.ticket == *ticket && store::ordered(&unit, &store)?,
        "stale create"
    );
    Ok(store)
}

fn lock(unit: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(unit.join("lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn create(req: &Value) -> Result<Value> {
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
    let unit_path = unit_dir(unit)?;
    let guard = lock(&unit_path)?;
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
    let root = match group::root() {
        Ok(root) => root,
        Err(error) => {
            return Ok(json!({"result":"definitely-not-sent","diagnostic":error.to_string()}))
        }
    };
    let generation = fresh();
    let token = format!("{}{}", fresh(), fresh());
    let ticket = json!({"version":2,"unit":unit,"attempt":attempt,"generation":generation,"nonce":fresh(),"capabilityHash":store::digest(token.as_bytes())});
    let run = unit_path.join(&generation);
    store::private_dir(&run)?;
    publish(&run.join("key.json"), &json!(token))?;
    let spec = json!({"ticket":ticket,"request":req});
    let mut cmd = store::stage_cmd("--bundled-guardian", &run, &spec, &token)?;
    cmd.env("WFC_BUNDLED_CGROUP_ROOT", root);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let creator = store::identity(std::process::id())?;
    let expires = unix_ms()? + (TIMEOUT + TIMEOUT).as_millis() as u64;
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            store.save_signed(
                "unstarted",
                json!({"reason":"guardian did not start","group_cleanup":"not-created"}),
            )?;
            return Ok(json!({"result":"definitely-not-sent","ticket":ticket}));
        }
    };
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

fn service_changed() -> Value {
    json!({"result":"uncertain","delivery":"definitely-not-sent","cleanup":"unknown","category":"bundled_restart","reconcile":true})
}

// The flag records whether the authenticated request frame may have left the client.
fn rpc(store: &Store, ready: &Value, req: &Value) -> (Value, bool) {
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
    guard.lock_shared()?;
    Ok((unit, guard))
}

/// A public close retry reads only the original execution's receipt. The send
/// flag lets the workflow release its intent after a confirmed pre-send failure.
pub(crate) fn assignment_operation(state: &Path, ticket: &Value, op: &str) -> (Value, bool) {
    let mut sent = false;
    let outcome = (|| -> Result<Value> {
        let (unit, _guard) = open_unit(state, field(ticket, "unit")?)?;
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
        if let Ok(proof) = target.proof() {
            if ordered && (!legacy || target.current_proof().is_ok()) {
                return Ok(terminal_reply(&target, proof, op));
            }
            return Ok(
                json!({"result":"uncertain","scope":"execution","cleanup":"unknown","ticket":ticket,
                "execution_cleanup":"confirmed","execution_proof":proof}),
            );
        }
        if op == "cleanup-receipt" || current.is_none() {
            return Ok(unknown_cleanup());
        }
        Ok(live_operation(
            &target,
            &json!({"op":op,"ticket":ticket}),
            op,
            !ordered || legacy,
            &mut sent,
        ))
    })();
    (outcome.unwrap_or_else(|_| unknown_cleanup()), sent)
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

fn operate(req: &Value) -> Result<Value> {
    let op = field(req, "op")?;
    if op == "create" {
        return create(req);
    }
    if op == "lookup" {
        let unit = unit_dir(field(req, "unit")?)?;
        let _guard = lock(&unit)?;
        return lookup(&unit, &req["attempt"]);
    }
    let ticket = &req["ticket"];
    let unit = unit_dir(field(ticket, "unit")?)?;
    let _guard = lock(&unit)?;
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

pub fn run(stage: &str) -> Result<u8> {
    let mut bytes = Vec::new();
    // Internal specs also carry the generated ticket around the caller's frame.
    let limit = wire::LIMIT + if stage == "--bundled" { 0 } else { 4096 };
    io::stdin().take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "oversized request");
    let request: Value = serde_json::from_slice(&bytes)?;
    match stage {
        "--bundled-guardian" | "--bundled-runtime" => {
            let spec = &request["spec"];
            let store = attached_store(spec)?;
            let grant = json!([stage, spec]);
            store::verify(
                &store.token,
                &serde_json::to_vec(&grant)?,
                field(&request, "signature")?,
            )?;
            // Consume the signed grant once, including if the stage dies while starting.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(store.run.join(stage))?;
            if stage == "--bundled-guardian" {
                runtime::guard(spec, store)?;
            } else {
                return runtime::serve(spec, store);
            }
        }
        _ => println!(
            "{}",
            operate(&request)
                .unwrap_or_else(|e| json!({"result":"refused","diagnostic":e.to_string()}))
        ),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

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
