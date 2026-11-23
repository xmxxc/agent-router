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
use store::{publish, read, Store};

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
    ensure!(
        read(&unit.join("current.json"))?["ticket"] == *ticket,
        "stale create"
    );
    Store::open(&unit, ticket)
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
    let root = match group::root() {
        Ok(root) => root,
        Err(error) => {
            return Ok(json!({"result":"definitely-not-sent","diagnostic":error.to_string()}))
        }
    };
    let unit_path = unit_dir(unit)?;
    let guard = lock(&unit_path)?;
    let current_path = unit_path.join("current.json");
    if current_path.try_exists()? {
        let current = read(&current_path)?;
        let old = &current["ticket"];
        if req["previous"] != *old || old["attempt"] == attempt {
            return Ok(json!({"result":"refused","diagnostic":"current create fence"}));
        }
        // Re-check the actual receipt even after a previously successful apply.
        let Ok(prior) = Store::open(&unit_path, old) else {
            return Ok(json!({"result":"uncertain","cleanup":"unknown"}));
        };
        if prior.signed("unstarted").is_err()
            && (current["applied"] != true || prior.proof().is_err())
        {
            return Ok(json!({"result":"uncertain","cleanup":"unknown"}));
        }
    } else {
        ensure!(req["previous"].is_null(), "missing previous create");
    }
    let generation = fresh();
    let token = format!("{}{}", fresh(), fresh());
    let ticket = json!({"unit":unit,"attempt":attempt,"generation":generation,"nonce":fresh(),"capabilityHash":store::digest(token.as_bytes())});
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
    publish(
        &current_path,
        &json!({"ticket":ticket,"applied":false,"creator":creator,"pending_until_ms":expires}),
    )?;
    let store = Store::open(&unit_path, &ticket)?;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            store.save_signed("unstarted", json!({"reason":"guardian did not start"}))?;
            return Ok(json!({"result":"definitely-not-sent","ticket":ticket}));
        }
    };
    drop(guard);
    let end = Instant::now() + TIMEOUT + TIMEOUT;
    loop {
        if let Ok(identity) = store.signed("ready") {
            return Ok(json!({"result":"accepted","ticket":ticket,"identity":identity}));
        }
        if store.signed("unstarted").is_ok() {
            return Ok(
                json!({"result":"definitely-not-sent","ticket":ticket,"diagnostic":"delegation unavailable"}),
            );
        }
        if child.try_wait()?.is_some() || Instant::now() >= end {
            return Ok(json!({"result":"uncertain","ticket":ticket}));
        }
        thread::sleep(TICK);
    }
}

fn rpc(store: &Store, ready: &Value, req: &Value) -> Value {
    let mut sent = false;
    let outcome = (|| -> Result<Value> {
        let service = &ready["service"];
        let pid = u32::try_from(service["pid"].as_u64().context("missing pid")?)?;
        ensure!(
            store::identity(pid)? == *service,
            "runtime identity changed"
        );
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
    outcome.unwrap_or_else(|_| json!({"result":if sent {"uncertain"} else {"definitely-not-sent"}, "cleanup":"unknown"}))
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
        let current = read(&unit.join("current.json"))?;
        ensure!(
            current["ticket"]["attempt"] == req["attempt"],
            "wrong attempt"
        );
        let store = Store::open(&unit, &current["ticket"]);
        let phase = match store {
            Err(_) => "unknown",
            Ok(s) if s.proof().is_ok() => "cleaned",
            Ok(s) if s.signed("unstarted").is_ok() => "unstarted",
            Ok(s) if s.signed("ready").is_ok() => "ready",
            Ok(_) => "pending",
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
    let mut current = read(&path)?;
    if current["ticket"] != *ticket {
        return Ok(json!({"result":"refused","diagnostic":"stale execution identity"}));
    }
    let Ok(store) = Store::open(&unit, ticket) else {
        return Ok(json!({"result":"uncertain","cleanup":"unknown"}));
    };
    if matches!(op, "cleanup-receipt" | "reconcile") {
        let Some(mut result) = completed(&store) else {
            return Ok(json!({"result":"uncertain","cleanup":"unknown"}));
        };
        if op == "reconcile" {
            let first = current["applied"] == false;
            if first {
                current["applied"] = json!(true);
                publish(&path, &current)?;
            }
            result["newly_applied"] = json!(first);
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
        Err(_) => return Ok(json!({"result":"uncertain","cleanup":"unknown"})),
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
                return Ok(json!({"result":"uncertain","cleanup":"unknown"}));
            }
            thread::sleep(TICK);
        }
    }
    if observing && answer["result"] != "refused" && answer["result"] != "accepted" {
        return Ok(
            completed(&store).unwrap_or_else(|| json!({"result":"uncertain","cleanup":"unknown"}))
        );
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
                runtime::guard(spec.clone())?;
            } else {
                return runtime::serve(spec.clone());
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
