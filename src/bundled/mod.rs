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
    ensure!(store.ticket == *ticket, "stale create");
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
        if req["previous"] != prior.ticket || prior.ticket["attempt"] == attempt {
            return Ok(json!({"result":"refused","diagnostic":"current create fence"}));
        }
        if !prior.replaceable(record) || store::check_legacy_siblings(&unit_path, prior).is_err() {
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

fn rpc(store: &Store, ready: &Value, req: &Value) -> Value {
    if store::process_state(&ready["service"]) != "alive" {
        return service_changed();
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
    outcome.unwrap_or_else(|_| {
        if !sent && store::process_state(&ready["service"]) != "alive" {
            return service_changed();
        }
        json!({"result":if sent {"uncertain"} else {"definitely-not-sent"}, "cleanup":"unknown"})
    })
}

fn unknown_cleanup() -> Value {
    json!({"result":"uncertain","cleanup":"unknown"})
}

fn completed(store: &Store) -> Option<Value> {
    store
        .proof()
        .ok()
        .map(|p| json!({"result":"completed","proof":p,"ticket":store.ticket}))
}

fn unix_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn operate(req: &Value) -> Result<Value> {
    let op = field(req, "op")?;
    if op == "create" {
        return create(req);
    }
    if op == "lookup" {
        let unit = unit_dir(field(req, "unit")?)?;
        let _guard = lock(&unit)?;
        let (current, store) = match store::current(&unit) {
            Ok(Some(c)) => c,
            Ok(None) => bail!("unknown unit"),
            Err(_) => return Ok(unknown_cleanup()),
        };
        ensure!(
            current["ticket"]["attempt"] == req["attempt"],
            "wrong attempt"
        );
        let phase = if store.proof().is_ok() {
            "cleaned"
        } else if store.run.join("cleanup.json").try_exists()? {
            "unknown"
        } else if store.signed("unstarted").is_ok() {
            "unstarted"
        } else if store.signed("ready").is_ok() {
            "ready"
        } else {
            "pending"
        };
        return Ok(
            json!({"result":"accepted","ticket":current["ticket"],"phase":phase,
            "creator":current["creator"],"creator_state":store::process_state(&current["creator"]),
            "pending_until_ms":current["pending_until_ms"],"pending_expired":phase=="pending" && unix_ms()? >= current["pending_until_ms"].as_u64().context("missing pending deadline")?}),
        );
    }
    let ticket = &req["ticket"];
    let unit = unit_dir(field(ticket, "unit")?)?;
    let guard = lock(&unit)?;
    let path = unit.join("current.json");
    let (mut current, store) = match store::current(&unit) {
        Ok(Some(c)) => c,
        Ok(None) => bail!("unknown unit"),
        Err(_) => return Ok(unknown_cleanup()),
    };
    if current["ticket"] != *ticket {
        return Ok(json!({"result":"refused","diagnostic":"stale execution identity"}));
    }
    if matches!(op, "cleanup-receipt" | "reconcile") {
        if store::check_legacy_siblings(&unit, &store).is_err() {
            return Ok(unknown_cleanup());
        }
        let Some(mut result) = completed(&store) else {
            return Ok(unknown_cleanup());
        };
        if op == "reconcile" {
            let proof = &result["proof"];
            let Ok(applied) = store.applied(&current, proof) else {
                return Ok(unknown_cleanup());
            };
            if !store.run.join("applied.json").try_exists()? {
                store.save_signed("applied", proof.clone())?;
            }
            if current["applied"] == false {
                if store.ticket["version"] == 2 {
                    current["appliedProof"] = json!(store::digest(&serde_json::to_vec(proof)?));
                }
                current["applied"] = json!(true);
                store::publish_current(&path, current, &store)?;
            }
            result["newly_applied"] = json!(!applied);
        }
        return Ok(result);
    }
    drop(guard);
    ensure!(
        matches!(op, "inspect" | "close" | "input" | "read" | "resize"),
        "unknown operation"
    );
    let observing = matches!(op, "inspect" | "close" | "read");
    if observing {
        if let Some(result) = completed(&store) {
            return Ok(result);
        }
    }
    let ready = match store.signed("ready") {
        Ok(v) => v,
        Err(_) => return Ok(unknown_cleanup()),
    };
    let mut answer = rpc(&store, &ready, req);
    if op == "inspect" && answer["result"] == "accepted" {
        answer["identity"] = ready;
    }
    if op == "close" && answer["result"] == "accepted" {
        let end = Instant::now() + TIMEOUT;
        loop {
            if let Some(result) = completed(&store) {
                return Ok(result);
            }
            if Instant::now() >= end {
                return Ok(unknown_cleanup());
            }
            thread::sleep(TICK);
        }
    }
    if observing && answer["result"] != "refused" && answer["result"] != "accepted" {
        return Ok(completed(&store).unwrap_or_else(|| {
            if answer["reconcile"] == true {
                answer
            } else {
                unknown_cleanup()
            }
        }));
    }
    Ok(answer)
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
                    let hello = socket.read(256,end).unwrap();
                    let client = field(&hello,"challenge").unwrap();
                    let challenge = fresh();
                    let key = if fault == "wrong-server" {"other"} else {token};
                    socket.write(&json!({"challenge":challenge,"proof":store::sign(key, format!("server:{client}:{challenge}").as_bytes())}),end).unwrap();
                    if fault == "wrong-server" {
                        assert!(socket.read(256,end).is_err());
                    } else {
                        let auth = socket.read(256,end).unwrap();
                        store::verify(token,format!("client:{client}:{challenge}").as_bytes(),field(&auth,"proof").unwrap()).unwrap();
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
                let answer = rpc(
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
                server.join().unwrap();
            });
        }
    }
}
