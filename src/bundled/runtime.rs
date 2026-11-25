use super::*;
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use wire::{Wire, FRAME, LIMIT, PREAUTH};

pub fn guard(
    state: &Path,
    spec: &Value,
    store: Store,
    managed: Option<(PathBuf, Value)>,
) -> Result<()> {
    let guardian = store::identity(std::process::id())?;
    let (root, user_service) = match managed {
        Some((root, unit)) => (Ok(root), Some(unit)),
        None => (group::root(), None),
    };
    let group = match root
        .map_err(group::AllocationFailure::from)
        .and_then(|root| group::Group::create(&root))
    {
        Ok(group) => group,
        Err(failure) => {
            store.save_signed(
                "unstarted",
                json!({"reason":format!("{:#}",failure.diagnostic),"group_cleanup":failure.group_cleanup,"path":failure.path}),
            )?;
            bail!("delegation unavailable")
        }
    };
    // From group creation onwards every return passes through empty+remove.
    let mut actual = json!({"guardian":guardian, "containment":group.identity});
    if let Some(binding) = &user_service {
        actual["userService"] = binding.clone();
    }
    let outcome = (|| -> Result<&str> {
        store.save_signed("starting", actual.clone())?;
        let (reader, writer) = UnixStream::pair()?;
        let mut cmd = store::stage_cmd("--bundled-runtime", &store.run, spec, &store.token)?;
        cmd.arg(state).stdout(OwnedFd::from(writer));
        let mut child = match group.launch(&mut cmd) {
            Ok(child) => child,
            Err(_) => return Ok("unstarted"),
        };
        actual["service"] = store::identity(child.id())?;
        let mut boot = Wire::new(reader);
        let started = (|| -> Result<()> {
            store.save_signed("starting", actual.clone())?;
            let details = boot.read(LIMIT, Instant::now() + TIMEOUT)?;
            if user_service.is_some() {
                // After ready, the original ticket owns the execution even
                // if the dispatch publisher later loses the reply.
                super::user_service::startup_live(&spec["userService"])?;
            }
            for key in ["kernel", "kernel_generation", "worker", "endpoint"] {
                actual[key] = details
                    .get(key)
                    .context("incomplete runtime ready")?
                    .clone();
            }
            store.save_signed("ready", actual.clone())
        })();
        if let Err(e) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        let status = child.wait()?;
        Ok(match status.code() {
            Some(0) => "closed",
            Some(10) => "exited",
            _ => "faulted",
        })
    })();
    let cleanup = group.clean();
    if matches!(outcome, Ok("unstarted")) {
        store.save_signed(
            "unstarted",
            json!({"reason":"runtime did not start","path":group.identity["path"],
            "group_cleanup":if cleanup.is_ok() {"removed"} else {"unknown"}}),
        )?;
        cleanup?;
        return Ok(());
    }
    cleanup?;
    store.save_signed(
        "cleanup",
        json!({"proves":"execution-empty","state":outcome.unwrap_or("faulted"), "identity":actual}),
    )
}

struct Kernel {
    channel: Wire<UnixStream>,
    child: Child,
    generation: String,
    unit: String,
    nonce: Value,
}

impl Kernel {
    fn start(spec: &Value) -> Result<Self> {
        let (client, child_io) = UnixStream::pair()?;
        let mut cmd = Command::new("/proc/self/exe");
        cmd.arg("--kernel")
            .stdin(OwnedFd::from(child_io.try_clone()?))
            .stdout(OwnedFd::from(child_io))
            .stderr(Stdio::null());
        // The public dispatcher supplies the worker environment. Never inherit
        // the launcher's controller credentials into a terminal worker.
        cmd.env_clear();
        for name in ["HOME", "PATH", "LANG", "LC_ALL", "TMPDIR", "USER"] {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        if let Some(env) = spec["request"]["env"].as_object() {
            for (k, v) in env {
                cmd.env(k, v.as_str().context("environment values must be strings")?);
            }
        }
        let child = cmd.spawn()?;
        let mut channel = Wire::new(client);
        let hello = channel.read(4096, Instant::now() + TIMEOUT)?;
        ensure!(
            hello["event"] == "hello" && hello["protocol"] == 1,
            "kernel handshake"
        );
        Ok(Self {
            child,
            channel,
            generation: field(&hello, "generation")?.to_owned(),
            unit: field(&spec["ticket"], "unit")?.to_owned(),
            nonce: Value::Null,
        })
    }

    fn call(&mut self, op: &str, mut request: Value) -> Result<Value> {
        let id = fresh();
        request["op"] = json!(op);
        request["generation"] = json!(self.generation);
        request["unit"] = json!(self.unit);
        request["nonce"] = self.nonce.clone();
        request["id"] = json!(id);
        let end = Instant::now() + TIMEOUT;
        self.channel.write(&request, end)?;
        let answer = self.channel.read(LIMIT, end)?;
        ensure!(
            answer["generation"] == self.generation && answer["id"] == id,
            "kernel response identity"
        );
        Ok(answer)
    }
}

fn receive(socket: &mut Wire<TcpStream>, token: &str) -> Result<Value> {
    let end = Instant::now() + PREAUTH;
    let hello = socket.read(256, end)?;
    let client = field(&hello, "challenge")?;
    ensure!(uuid::Uuid::parse_str(client).is_ok(), "challenge");
    let challenge = fresh();
    let proof = store::sign(token, format!("server:{client}:{challenge}").as_bytes());
    socket.write(&json!({"challenge":challenge,"proof":proof}), end)?;
    let auth = socket.read(256, end)?;
    store::verify(
        token,
        format!("client:{client}:{challenge}").as_bytes(),
        field(&auth, "proof")?,
    )?;
    socket.read(LIMIT, Instant::now() + FRAME)
}

pub fn serve(spec: &Value, store: Store) -> Result<u8> {
    if spec["userService"].is_object() {
        user_service::startup_live(&spec["userService"])?;
    }
    let mut kernel = Kernel::start(spec)?;
    let spawn = &spec["request"];
    // Only argv/cwd belong on the kernel wire.
    let result = kernel.call(
        "spawn",
        json!({"argv":spawn["argv"],"cwd":spawn["cwd"],"cols":120,"rows":40}),
    )?;
    ensure!(result["ok"] == true, "kernel spawn failed");
    kernel.nonce = json!(field(&result["result"], "nonce")?);
    let pid = u32::try_from(result["result"]["pid"].as_u64().context("worker pid")?)?;
    // A short-lived worker remains a child/zombie until the first kernel read.
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let details = json!({"kernel":store::identity(kernel.child.id())?,"kernel_generation":kernel.generation,
        "worker":store::identity(pid)?, "endpoint":listener.local_addr()?.port()});
    writeln!(io::stdout().lock(), "{details}")?;
    let mut next_poll = Instant::now();
    let mut last_input = 0u64;
    loop {
        if Instant::now() >= next_poll {
            let view = kernel.call("read", json!({}))?;
            ensure!(
                view["ok"] == true && view["result"]["read_error"].is_null(),
                "terminal observation failed"
            );
            if !view["result"]["exit"].is_null() {
                return Ok(10);
            }
            next_poll = Instant::now() + Duration::from_millis(200);
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(TICK);
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut socket = Wire::new(stream);
        let Ok(req) = receive(&mut socket, &store.token) else {
            continue;
        };
        let op = req["op"].as_str().unwrap_or("");
        let owned = req["ticket"] == store.ticket
            && req["id"]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok());
        let mut reply = if !owned {
            json!({"result":"refused"})
        } else {
            match op {
                "close" => json!({"result":"accepted"}),
                "input"
                    if last_input.checked_add(1) != req["seq"].as_u64()
                        || req["seq"].as_u64().is_none()
                        || !req["text"].is_string() =>
                {
                    json!({"result":"refused"})
                }
                "input" | "read" | "inspect" | "resize" => {
                    let (kernel_op, data) = match op {
                        "input" => {
                            // Reserve before touching the PTY. A lost acknowledgement never replays bytes.
                            last_input += 1;
                            ("input", json!({"text":req["text"]}))
                        }
                        "resize" => ("resize", json!({"cols":req["cols"],"rows":req["rows"]})),
                        _ => ("read", json!({})),
                    };
                    let answer = kernel.call(kernel_op, data)?;
                    if answer["ok"] == true {
                        json!({"result":"accepted","data":answer["result"],"last_input":last_input})
                    } else if op == "input" {
                        json!({"result":"uncertain"})
                    } else {
                        json!({"result":"refused"})
                    }
                }
                _ => json!({"result":"refused"}),
            }
        };
        reply["id"] = req["id"].clone();
        reply["ticket"] = store.ticket.clone();
        let _ = socket.write(&reply, Instant::now() + FRAME);
        if owned && op == "close" {
            return Ok(0);
        }
    }
}
