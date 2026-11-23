#![cfg(target_os = "linux")]
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
use uuid::Uuid;

fn command(state: &Path, root: &Path) -> Command {
    let mut cmd = Command::new(
        std::env::var_os("WFC_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_wfc").into()),
    );
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("WFC_CONTROLLER_TOKEN", "test-controller")
        .env("WFC_OWNER_TOKEN", "test-owner")
        .env("WFC_WORKER_CALLBACK", "test-old-callback")
        .env("wFc_OwNeR_ToKeN", "test-mixed-case")
        .arg("--bundled")
        .env("WFC_STATE", state)
        .env("WFC_BUNDLED_CGROUP_ROOT", root)
        .stdin(Stdio::piped());
    cmd
}

fn call(state: &Path, root: &Path, request: Value) -> Value {
    let mut child = command(state, root).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn ordinary_directory_cannot_launch() {
    let dir = std::env::temp_dir().join(format!("wfc-reject-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    for name in [
        "cgroup.controllers",
        "cgroup.kill",
        "cgroup.procs",
        "cgroup.events",
    ] {
        fs::write(dir.join(name), "").unwrap();
    }
    let request = json!({"op":"create","unit":"test","attempt":"a", "cwd":dir,"argv":["/bin/touch","started"]});
    let response = call(&dir.join("state"), &dir, request);
    assert_eq!(response["result"], "definitely-not-sent", "{response}");
    assert!(!dir.join("started").exists());
    fs::remove_dir_all(dir).unwrap();
}

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

fn until(mut observe: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !observe() {
        assert!(Instant::now() < end, "observation timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

// Exercise the disconnect after a real request reaches the runtime. Inspect
// the PTY worker's own output file to count effects independently of the ACK.
fn drop_reply(state: &Path, created: &Value) {
    let ticket = &created["ticket"];
    let port = created["identity"]["endpoint"].as_u64().unwrap() as u16;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    let key = state
        .join("bundled/test")
        .join(ticket["generation"].as_str().unwrap())
        .join("key.json");
    let key: String = serde_json::from_slice(&fs::read(key).unwrap()).unwrap();
    let client = Uuid::new_v4().to_string();
    writeln!(stream, "{}", json!({"challenge":client})).unwrap();
    let mut hello = String::new();
    BufReader::new(&stream).read_line(&mut hello).unwrap();
    let hello: Value = serde_json::from_str(&hello).unwrap();
    let server = hello["challenge"].as_str().unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(format!("client:{client}:{server}").as_bytes());
    writeln!(
        stream,
        "{}",
        json!({"proof":format!("{:x}",mac.finalize().into_bytes())})
    )
    .unwrap();
    writeln!(stream,"{}",json!({"op":"input","seq":1,"text":"once\n","id":Uuid::new_v4().to_string(),"ticket":ticket})).unwrap();
}

// Block only the launcher's stdout, so SIGKILL really lands after launch
// and before the caller receives a handle. The independent CLI recovers it.
fn lose_launcher(state: &Path, root: &Path, create: &Value) -> Value {
    use std::os::fd::OwnedFd;
    use std::os::unix::{net::UnixStream, process::ExitStatusExt};
    let (_reader, mut writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    while writer.write(&[0; 4096]).is_ok() {}
    while writer.write(&[0]).is_ok() {}
    writer.set_nonblocking(false).unwrap();
    let mut child = command(state, root)
        .stdout(OwnedFd::from(writer))
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{create}").unwrap();
    let current = state.join("bundled/test/current.json");
    until(|| current.exists());
    let lookup = call(
        state,
        root,
        json!({"op":"lookup","unit":"test","attempt":create["attempt"]}),
    );
    assert_eq!(lookup["creator_state"], "alive");
    assert!(lookup["pending_until_ms"].is_u64());
    let ticket = &lookup["ticket"];
    let ready = current
        .parent()
        .unwrap()
        .join(ticket["generation"].as_str().unwrap())
        .join("ready.json");
    until(|| ready.exists());
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let recovered = call(
        state,
        root,
        json!({"op":"lookup","unit":"test","attempt":create["attempt"]}),
    );
    assert_eq!(recovered["creator_state"], "exited");
    assert_eq!(recovered["phase"], "ready");
    call(state, root, json!({"op":"inspect","ticket":ticket}))
}

fn verify_stage_environment(info: &Value) {
    for role in ["guardian", "service", "kernel", "worker"] {
        let pid = info[role]["pid"].as_u64().unwrap();
        let bytes = fs::read(format!("/proc/{pid}/environ")).unwrap();
        let mut callback = None;
        for item in bytes.split(|b| *b == 0) {
            let Some(i) = item.iter().position(|b| *b == b'=') else {
                continue;
            };
            let name = &item[..i];
            assert!(
                !name.eq_ignore_ascii_case(b"WFC_CONTROLLER_TOKEN")
                    && !name.eq_ignore_ascii_case(b"WFC_OWNER_TOKEN"),
                "{role} inherited a control credential"
            );
            if name.eq_ignore_ascii_case(b"WFC_WORKER_CALLBACK") {
                callback = Some(&item[i + 1..]);
            }
        }
        let expected =
            matches!(role, "kernel" | "worker").then_some(b"test-current-callback".as_slice());
        assert_eq!(callback, expected, "callback at {role}");
    }
}

fn sign_fixture(body: &Value, key: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(&serde_json::to_vec(body).unwrap());
    serde_json::to_vec(
        &json!({"body":body,"signature":format!("{:x}",mac.finalize().into_bytes())}),
    )
    .unwrap()
}

fn verify_history_recovery(state: &Path, root: &Path, created: &Value, create: &Value) {
    let index = state.join("bundled/test/current.json");
    let original = fs::read(&index).unwrap();
    let mut changed: Value = serde_json::from_slice(&original).unwrap();
    changed["ticket"]["attempt"] = json!("other");
    let altered = serde_json::to_vec(&changed).unwrap();
    fs::remove_file(&index).unwrap();
    for corrupt in [None, Some(b"{".as_slice()), Some(altered.as_slice())] {
        if let Some(bytes) = corrupt {
            fs::write(&index, bytes).unwrap();
        }
        for req in [
            create.clone(),
            json!({"op":"lookup","unit":"test","attempt":"closed"}),
            json!({"op":"reconcile","ticket":created["ticket"]}),
        ] {
            assert_eq!(call(state, root, req)["cleanup"], "unknown");
        }
    }
    let backup = state
        .join("bundled/test")
        .join(created["ticket"]["generation"].as_str().unwrap())
        .join("index.json");
    assert_eq!(fs::read(&backup).unwrap(), original);
    fs::copy(backup, index).unwrap();
    let inspect = call(
        state,
        root,
        json!({"op":"inspect","ticket":created["ticket"]}),
    );
    assert_eq!(inspect["identity"], created["identity"]);
}

#[test]
#[ignore = "requires WFC_TEST_DELEGATION pointing at a disposable writable cgroup v2 delegation"]
fn delegated_lifecycle() {
    let root = std::path::PathBuf::from(
        std::env::var_os("WFC_TEST_DELEGATION").expect("set WFC_TEST_DELEGATION"),
    );
    let dir = std::env::temp_dir().join(format!("wfc-native-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let state = dir.join("state");
    if let Some(denied) = std::env::var_os("WFC_TEST_UNDELEGATED") {
        let refused = call(
            &state,
            Path::new(&denied),
            json!({"op":"create","unit":"denied","attempt":"a","cwd":dir,"argv":["/bin/touch","started"]}),
        );
        assert_eq!(refused["result"], "definitely-not-sent", "{refused}");
        assert!(!dir.join("started").exists());
    }
    let backend = |request| call(&state, &root, request);
    let worker = r#"cat /proc/self/cgroup > membership
setsid sh -c 'echo $$ > descendant; exec sleep 180' </dev/null >/dev/null 2>&1 &
yes 01234567890123456789012345678901234567890123456789012345678901234567890123456789 | head -c 1048576
while read -r line; do
  echo "$line" >> observed
  if [ "$line" = exit ]; then
    while [ ! -f trigger ]; do sleep 0.05; done
    exit 0
  fi
done"#;
    let mut previous = Value::Null;
    let mut old_receipt = Vec::new();
    for cause in ["closed", "exited", "faulted"] {
        let create = json!({"op":"create","unit":"test","attempt":cause,"previous":previous,"cwd":dir,"env":{"WFC_WORKER_CALLBACK":"test-current-callback"},"argv":["/bin/sh","-c",worker]});
        let created = if cause == "closed" {
            lose_launcher(&state, &root, &create)
        } else {
            backend(create.clone())
        };
        assert_eq!(created["result"], "accepted", "{created}");
        let ticket = &created["ticket"];
        let info = &created["identity"];
        let invoke = |op| backend(json!({"op":op,"ticket":ticket}));
        verify_stage_environment(info);
        if cause == "closed" {
            verify_history_recovery(&state, &root, &created, &create);
        }
        let path = Path::new(info["containment"]["path"].as_str().unwrap());
        let group = path.file_name().unwrap().to_str().unwrap();
        let members = fs::read_to_string(path.join("cgroup.procs")).unwrap();
        for key in ["service", "kernel", "worker"] {
            let pid = info[key]["pid"].as_u64().unwrap().to_string();
            assert!(members.lines().any(|s| s == pid), "missing {key} {pid}");
            assert!(info[key]["start_ticks"].is_u64());
        }
        until(|| dir.join("descendant").exists());
        let descendant = fs::read_to_string(dir.join("descendant")).unwrap();
        assert!(fs::read_to_string(path.join("cgroup.procs"))
            .unwrap()
            .lines()
            .any(|s| s == descendant.trim()));
        assert!(fs::read_to_string(dir.join("membership"))
            .unwrap()
            .trim()
            .ends_with(group));
        let nested = path.join("inner");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("cgroup.procs"), descendant.trim()).unwrap();
        assert_eq!(invoke("cleanup-receipt")["cleanup"], "unknown");
        let receipt = state
            .join("bundled/test")
            .join(ticket["generation"].as_str().unwrap())
            .join("cleanup.json");
        if !old_receipt.is_empty() {
            fs::write(&receipt, &old_receipt).unwrap();
            assert_eq!(invoke("reconcile")["cleanup"], "unknown");
            fs::remove_file(&receipt).unwrap();
        }
        if cause == "closed" {
            let ready = receipt.with_file_name("ready.json");
            let original = fs::read(&ready).unwrap();
            let key: String =
                serde_json::from_slice(&fs::read(receipt.with_file_name("key.json")).unwrap())
                    .unwrap();
            let mut body = serde_json::from_slice::<Value>(&original).unwrap()["body"].clone();
            let ticks = body["data"]["service"]["start_ticks"].as_u64().unwrap();
            body["data"]["service"]["start_ticks"] = json!(ticks + 1);
            fs::write(&ready, sign_fixture(&body, &key)).unwrap();
            let stale = backend(json!({"op":"input","ticket":ticket,"seq":1,"text":"never"}));
            assert_eq!(stale["result"], "uncertain");
            assert_eq!(stale["delivery"], "definitely-not-sent");
            assert_eq!(stale["reconcile"], true);
            fs::write(ready, original).unwrap();
        }
        for field in ["generation", "nonce", "attempt"] {
            let mut bad = ticket.clone();
            bad[field] = json!("wrong");
            assert_eq!(
                backend(json!({"op":"input","ticket":bad,"text":"bad\n","seq":1}))["result"],
                "refused"
            );
        }
        let port = info["endpoint"].as_u64().unwrap() as u16;
        let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
        slow.write_all(b"{").unwrap();
        assert_eq!(invoke("read")["result"], "accepted");
        drop(slow);
        drop_reply(&state, &created);
        until(|| fs::read_to_string(dir.join("observed")).is_ok_and(|s| s == "once\n"));
        assert_eq!(
            backend(json!({"op":"input","ticket":ticket,"seq":1,"text":"once\n"}))["result"],
            "refused"
        );
        match cause {
            "closed" => assert_eq!(invoke("close")["result"], "completed"),
            "exited" => {
                assert_eq!(
                    backend(json!({"op":"input","ticket":ticket,"seq":2,"text":"exit\n"}))
                        ["result"],
                    "accepted"
                );
                let clients: Vec<_> = (0..12)
                    .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
                    .collect();
                fs::write(dir.join("trigger"), "").unwrap();
                until(|| receipt.exists());
                drop(clients);
            }
            _ => {
                let pid = info["service"]["pid"].as_i64().unwrap() as i32;
                assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            }
        }
        until(|| receipt.exists());
        assert!(!path.exists());
        let dead = fs::read_to_string(format!("/proc/{}/stat", descendant.trim()));
        assert!(dead.map_or(true, |s| s.contains(") Z ")));
        assert_eq!(
            fs::read_to_string(dir.join("observed"))
                .unwrap()
                .lines()
                .filter(|s| *s == "once")
                .count(),
            1
        );
        let result = invoke("cleanup-receipt");
        assert_eq!(result["proof"]["identity"], *info);
        assert_eq!(result["proof"]["state"], cause);
        assert_eq!(result["proof"]["proves"], "execution-empty");
        let first = thread::scope(|s| {
            let jobs: Vec<_> = (0..4).map(|_| s.spawn(|| invoke("reconcile"))).collect();
            jobs.into_iter()
                .map(|j| {
                    let r = j.join().unwrap();
                    assert_eq!(r["result"], "completed");
                    assert_eq!(r["proof"], result["proof"]);
                    assert!(r["newly_applied"].is_boolean());
                    r
                })
                .filter(|r| r["newly_applied"] == true)
                .count()
        });
        assert_eq!(first, 1);
        assert_eq!(invoke("reconcile")["newly_applied"], false);
        if cause == "closed" {
            let marker = receipt.with_file_name("applied.json");
            fs::remove_file(&marker).unwrap();
            assert_eq!(invoke("reconcile")["newly_applied"], false);
            assert!(marker.exists());
            let index = state.join("bundled/test/current.json");
            fs::copy(receipt.with_file_name("index.json"), index).unwrap();
            assert_eq!(invoke("reconcile")["newly_applied"], false);
        }
        old_receipt = fs::read(&receipt).unwrap();
        if std::env::var_os("WFC_TEST_PRINT_RECEIPTS").is_some() {
            eprintln!("verified_receipt {}", String::from_utf8_lossy(&old_receipt));
        }
        let mut tampered: Value = serde_json::from_slice(&old_receipt).unwrap();
        tampered["body"]["data"]["identity"]["guardian"]["pid"] = json!(1);
        let changed = serde_json::to_vec(&tampered).unwrap();
        if cause == "closed" {
            let key: String =
                serde_json::from_slice(&fs::read(receipt.with_file_name("key.json")).unwrap())
                    .unwrap();
            for pointer in [
                "/data/state",
                "/data/identity/service/start_ticks",
                "/data/identity/kernel/pid",
                "/data/identity/guardian/pid",
                "/data/identity/containment/inode",
                "/data/identity/worker/boot",
            ] {
                let mut body: Value =
                    serde_json::from_slice::<Value>(&old_receipt).unwrap()["body"].clone();
                let slot = body.pointer_mut(pointer).unwrap();
                *slot = if let Some(n) = slot.as_u64() {
                    json!(n + 1)
                } else {
                    json!("active")
                };
                fs::write(&receipt, sign_fixture(&body, &key)).unwrap();
                assert_eq!(invoke("reconcile")["cleanup"], "unknown", "{pointer}");
                assert_eq!(
                    backend(
                        json!({"op":"create","unit":"test","previous":ticket,"attempt":"conflict","cwd":dir,"argv":["/bin/true"]})
                    )["cleanup"],
                    "unknown",
                    "{pointer}"
                );
            }
        }
        fs::write(&receipt, &old_receipt).unwrap();
        for op in ["input", "resize"] {
            let answer = backend(
                json!({"op":op,"ticket":ticket,"seq":2,"text":"never","cols":55,"rows":20}),
            );
            assert_eq!(answer["result"], "definitely-not-sent");
            assert_eq!(answer["cleanup"], "confirmed");
            assert_eq!(answer["proof"], result["proof"]);
            assert_eq!(answer["delivery"], "definitely-not-sent");
            assert_ne!(answer["reconcile"], true);
        }
        for corrupt in [changed.as_slice(), b"{}".as_slice(), b"broken"] {
            fs::write(&receipt, corrupt).unwrap();
            assert_eq!(invoke("inspect")["cleanup"], "unknown");
            assert_eq!(
                backend(
                    json!({"op":"create","unit":"test","previous":ticket,"attempt":"forged","cwd":dir,"argv":["/bin/true"]})
                )["result"],
                "uncertain"
            );
        }
        fs::write(&receipt, &old_receipt).unwrap();
        let key = receipt.with_file_name("key.json");
        let backup = fs::read(&key).unwrap();
        fs::remove_file(&key).unwrap();
        for op in ["inspect", "close", "reconcile", "cleanup-receipt"] {
            assert_eq!(invoke(op)["cleanup"], "unknown");
        }
        fs::write(key, backup).unwrap();
        previous = ticket.clone();
        for file in ["membership", "descendant", "observed"] {
            fs::remove_file(dir.join(file)).unwrap();
        }
        eprintln!("verified {cause}: {group} removed, receipt bound, input counted once");
    }
    let early = backend(
        json!({"op":"create","unit":"early","attempt":"missing-worker","cwd":dir,"argv":["/nonexistent-wfc-test-program"]}),
    );
    let early_ticket = &early["ticket"];
    assert_eq!(early["result"], "uncertain");
    // Under an unprivileged delegation the marker can commit even when
    // its current-index mirror cannot. A retry only repairs the mirror.
    let unit = state.join("bundled/early");
    let deny_mirror = unsafe { libc::geteuid() } != 0;
    if deny_mirror {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unit, fs::Permissions::from_mode(0o500)).unwrap();
        let partial = backend(json!({"op":"reconcile","ticket":early_ticket}));
        fs::set_permissions(&unit, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(partial["result"], "uncertain");
        assert_eq!(partial["recovery_pending"], true);
        assert_eq!(partial["newly_applied"], true);
        assert_eq!(partial["applied"], true);
        assert_eq!(partial["proof"]["state"], "faulted");
    }
    let early_proof = backend(json!({"op":"reconcile","ticket":early_ticket}));
    assert_eq!(early_proof["proof"]["state"], "faulted", "{early_proof}");
    assert!(early_proof["proof"]["identity"]["service"].is_object());
    assert!(early_proof["proof"]["identity"]["kernel"].is_null());
    assert_eq!(early_proof["newly_applied"], !deny_mirror);
    assert_eq!(early_proof["recovery_pending"], false);
    assert_eq!(
        backend(json!({"op":"reconcile","ticket":early_ticket}))["newly_applied"],
        false
    );
    eprintln!("verified pre-ready fault, signed identity and single apply");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lost_or_damaged_current_keeps_history_fenced() {
    let state = std::env::temp_dir().join(format!("wfc-history-{}", Uuid::new_v4()));
    let unit = state.join("bundled/history");
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(unit.join(Uuid::new_v4().to_string()))
        .unwrap();
    let current = unit.join("current.json");
    for contents in [None, Some("broken"), Some("{}")] {
        if let Some(bytes) = contents {
            fs::write(&current, bytes).unwrap();
        }
        for req in [
            json!({"op":"lookup","unit":"history","attempt":"a"}),
            json!({"op":"reconcile","ticket":{"unit":"history"}}),
            json!({"op":"create","unit":"history","attempt":"b","cwd":state,"argv":["/bin/true"]}),
        ] {
            let answer = call(&state, &state, req);
            assert_eq!(answer["result"], "uncertain", "{answer}");
            assert_eq!(answer["cleanup"], "unknown");
        }
    }
    fs::remove_dir_all(state).unwrap();
}
