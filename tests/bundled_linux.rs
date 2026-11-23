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
    cmd.arg("--bundled")
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
        let create = json!({"op":"create","unit":"test","attempt":cause,"previous":previous,"cwd":dir,"argv":["/bin/sh","-c",worker]});
        let created = if cause == "closed" {
            lose_launcher(&state, &root, &create)
        } else {
            backend(create)
        };
        assert_eq!(created["result"], "accepted", "{created}");
        let ticket = &created["ticket"];
        let info = &created["identity"];
        let invoke = |op| backend(json!({"op":op,"ticket":ticket}));
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
        old_receipt = fs::read(&receipt).unwrap();
        if std::env::var_os("WFC_TEST_PRINT_RECEIPTS").is_some() {
            eprintln!("verified_receipt {}", String::from_utf8_lossy(&old_receipt));
        }
        let mut tampered: Value = serde_json::from_slice(&old_receipt).unwrap();
        tampered["body"]["data"]["identity"]["guardian"]["pid"] = json!(1);
        let changed = serde_json::to_vec(&tampered).unwrap();
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
    fs::remove_dir_all(dir).unwrap();
}
