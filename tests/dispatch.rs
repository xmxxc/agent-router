use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::Command};
use uuid::Uuid;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wfc-dispatch-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn wfc(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wfc"));
        command.current_dir(&self.0).env("WFC_STATE", "state");
        command
    }

    fn dispatch(&self, cwd: &str, role: &str, task: &str) -> Command {
        fs::create_dir(self.0.join(cwd)).unwrap();
        fs::write(self.0.join(task), format!("instructions for {role}")).unwrap();
        let mut command = self.wfc();
        command.args([
            "dispatch", "--cwd", cwd, "--task", task, "--role", role, "--",
        ]);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn run(command: &mut Command) -> Value {
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

// On Linux the handoff receipt precedes the worker's exit. Compare all
// dispatch fields after waiting for the independent process outcome.
fn completed(f: &Fixture, mut receipt: Value, code: i32) -> Value {
    if cfg!(target_os = "linux") {
        let id = receipt["dispatch"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let row = run(f.wfc().args(["status", "--dispatch", id]))["dispatches"][0].clone();
            if row["worker"]["state"] != "running" {
                receipt["worker"] = json!({"state":"exited","exit_code":code,"signal":null});
                for (field, value) in receipt.as_object().unwrap() {
                    assert_eq!(&row[field], value);
                }
                return row;
            }
            assert!(std::time::Instant::now() < deadline, "{row}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    run(f.wfc().args([
        "status",
        "--dispatch",
        receipt["dispatch"].as_str().unwrap(),
    ]))["dispatches"][0]
        .clone()
}

const PYTHON: &str = if cfg!(windows) { "python" } else { "python3" };
const WORKER: &str = r#"
import json, os, sys
from pathlib import Path
env = os.environ
task = Path(env['WFC_TASK'])
receipt = dict(role=env['WFC_ROLE'], body=task.read_text(encoding='utf-8'), arg=sys.argv[1],
               dispatch=env['WFC_DISPATCH'], round=int(env['WFC_ROUND']), attempt=env['WFC_ATTEMPT'])
Path('receipt.json').write_text(json.dumps(receipt))
print('worker output')
"#;

#[test]
fn worker_receives_context_and_a_fresh_cli_finds_the_same_dispatch() {
    let f = Fixture::new();
    let role = "实现 reviewer";
    let literal = "one arg & two";
    let record = run(f
        .dispatch("work dir", role, "task one.md")
        .args([PYTHON, "-c", WORKER, literal]));
    let record = completed(&f, record, 0);
    let receipt: Value =
        serde_json::from_slice(&fs::read(f.0.join("work dir/receipt.json")).unwrap()).unwrap();
    assert_eq!(record["role"], role);
    assert_eq!(record["round"], 1);
    assert!(record["dispatch"].as_str().unwrap().starts_with("d-"));
    assert!(record["attempt"].as_str().unwrap().starts_with("a-"));
    assert_eq!(
        receipt,
        json!({
            "body": format!("instructions for {role}"), "arg": literal, "role": role,
            "dispatch": record["dispatch"], "round": 1, "attempt": record["attempt"],
        })
    );
    let status = run(f
        .wfc()
        .args(["status", "--dispatch", record["dispatch"].as_str().unwrap()]));
    assert_eq!(status["dispatches"], json!([record]));
}

#[test]
fn overlapping_workers_keep_separate_records() {
    let f = Fixture::new();
    // Both workers must reach their own working directory before either can return.
    let worker = format!(
        r#"
from pathlib import Path
import time, sys
Path('ready').touch()
deadline = time.monotonic() + 10
while not (Path('..') / sys.argv[1] / 'ready').exists():
    if time.monotonic() > deadline:
        raise RuntimeError('other worker did not start')
    time.sleep(0.02)
{WORKER}"#
    );
    let mut first = f.dispatch("left", "writer", "first.md");
    let mut second = f.dispatch("right", "reader", "second.md");
    first.args([PYTHON, "-c", &worker, "right"]);
    second.args([PYTHON, "-c", &worker, "left"]);
    let (a, b) = std::thread::scope(|scope| {
        let other = scope.spawn(|| run(&mut second));
        (run(&mut first), other.join().unwrap())
    });
    assert_ne!(a["dispatch"], b["dispatch"]);
    assert_ne!(a["attempt"], b["attempt"]);
    let (a, b) = (completed(&f, a, 0), completed(&f, b, 0));
    let all = run(f.wfc().arg("status"));
    let rows = all["dispatches"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (record, dir, task, role) in [
        (a, "left", "first.md", "writer"),
        (b, "right", "second.md", "reader"),
    ] {
        let listed = rows
            .iter()
            .find(|r| r["dispatch"] == record["dispatch"])
            .unwrap();
        assert_eq!(listed["worker"], record["worker"]);
        let id = record["dispatch"].as_str().unwrap();
        let one = run(f.wfc().args(["status", "--dispatch", id]));
        assert_eq!(one["dispatches"], json!([record]));
        let receipt: Value =
            serde_json::from_slice(&fs::read(f.0.join(dir).join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["dispatch"], id);
        assert_eq!(receipt["attempt"], record["attempt"]);
        assert_eq!(receipt["role"], role);
        assert_eq!(receipt["body"], format!("instructions for {role}"));
        assert_eq!(
            record["task"],
            f.0.join(task).canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(
            record["cwd"],
            f.0.join(dir).canonicalize().unwrap().to_str().unwrap()
        );
    }
}

#[test]
fn spawn_errors_and_worker_exit_codes_are_distinguished() {
    let f = Fixture::new();
    assert_eq!(run(f.wfc().arg("status"))["dispatches"], json!([]));
    assert!(!f.0.join("state").exists());
    let good = run(f
        .dispatch("ok", "tester", "ok.md")
        .args([PYTHON, "-c", "pass"]));
    let missing = f
        .dispatch("missing", "tester", "one.md")
        .arg(f.0.join("no such worker"))
        .output()
        .unwrap();
    let failed = f
        .dispatch("fails", "tester", "two.md")
        .args([PYTHON, "-c", "raise SystemExit(7)"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(!missing.stderr.is_empty());
    let good = completed(&f, good, 0);
    if cfg!(target_os = "linux") {
        assert!(failed.status.success());
        let bad = completed(&f, serde_json::from_slice(&failed.stdout).unwrap(), 7);
        let all = run(f.wfc().arg("status"));
        let rows = all["dispatches"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for expected in [good, bad] {
            assert!(rows.iter().any(
                |r| r["dispatch"] == expected["dispatch"] && r["worker"] == expected["worker"]
            ));
        }
    } else {
        assert!(!failed.status.success());
        assert!(failed.stdout.is_empty());
        assert!(!failed.stderr.is_empty());
        assert_eq!(
            run(f.wfc().arg("status"))["dispatches"][0]["dispatch"],
            good["dispatch"]
        );
    }
    let unknown = f
        .wfc()
        .args(["status", "--dispatch", "d-00000000000000000000000000000000"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn detached_worker_can_write_after_dispatch_exits() {
    let f = Fixture::new();
    let record = run(f.dispatch("work", "worker", "task.md").args([
        "sh", "-c",
        "for i in $(seq 100); do if test -e release; then touch finished; exit 0; fi; sleep .02; done; exit 7",
    ]));
    fs::write(f.0.join("work/release"), "").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f.0.join("work/finished").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let close = run(f
        .wfc()
        .args(["close", record["dispatch"].as_str().unwrap()]));
    assert_eq!(close["result"], "completed");
}

#[cfg(target_os = "linux")]
#[test]
fn closing_a_worker_preserves_a_separately_dispatched_nested_worker() {
    let f = Fixture::new();
    let binary = env!("CARGO_BIN_EXE_wfc");
    let parent = run(f.dispatch("work", "parent", "task.md").args([
        "sh", "-c",
        r#""$1" dispatch --role nested --task "$WFC_TASK" --cwd . -- sh -c 'for i in $(seq 500); do if test -f release; then exit 3; fi; sleep .02; done; exit 7' > nested.tmp && mv nested.tmp nested.json; exec sleep 10"#,
        "worker", binary,
    ]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let receipt = f.0.join("work/nested.json");
    while !receipt.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let nested = serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
    run(f
        .wfc()
        .args(["close", parent["dispatch"].as_str().unwrap()]));
    fs::write(f.0.join("work/release"), "").unwrap();
    completed(&f, nested, 3);
}

#[cfg(target_os = "linux")]
#[test]
fn close_reaps_an_orphan_in_another_session_and_preserves_the_root_exit() {
    let f = Fixture::new();
    let receipt = run(f.dispatch("work", "worker", "task.md").args([
        PYTHON,
        "-c",
        r#"
import os, sys, time
from pathlib import Path
parent = os.getpid()
if os.fork():
    os._exit(7)
os.setsid()
while os.getppid() == parent:
    time.sleep(.01)
Path('pid.tmp').write_text(str(os.getpid()))
Path('pid.tmp').rename('adopted')
time.sleep(10)
"#,
    ]));
    let path = f.0.join("work/adopted");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid = fs::read_to_string(path).unwrap();
    let id = receipt["dispatch"].as_str().unwrap();
    for _ in 0..2 {
        let close = run(f.wfc().args(["close", id]));
        assert_eq!(close["result"], "completed");
        assert_eq!(close["attempt"], receipt["attempt"]);
        assert!(!std::path::Path::new("/proc").join(&pid).exists());
    }
    completed(&f, receipt, 7);
}

#[cfg(target_os = "linux")]
#[test]
fn archived_short_id_keeps_rounds_and_completed_result() {
    let f = Fixture::new();
    let dir = f.0.join("state/dispatches");
    fs::create_dir_all(&dir).unwrap();
    let id = "d-12345678";
    let attempt = "a-abcdef012345";
    let first = json!({"round":1,"attempt":"a-012345abcdef","state":"completed","result":{"report":"first"}});
    let message = json!({"id":3,"messageId":"m-original","dispatch":id,"type":"done","attempt":attempt,"round":2,"report":"second","at":"yesterday","head":"original","dirty":false,"to":"controller","state":"received"});
    let created = 1_795_433_296.589_775_3_f64;
    let row = json!({"created":created,"dispatch":id,"round":2,"attempt":attempt,"task":"task","report":"second","rounds":[first],"messages":[message]});
    fs::write(dir.join(format!("{id}.json")), row.to_string()).unwrap();
    for _ in 0..2 {
        let status = run(f.wfc().args(["status", "--dispatch", id]));
        let record = &status["dispatches"][0];
        assert_eq!(record["created"].as_f64(), Some(created));
        assert_eq!(record["state"], "done");
        assert_eq!(record["rounds"][0], first);
        assert_eq!(record["rounds"][1]["result"]["messageId"], "m-original");
        assert_eq!(record["messages"][0], message);
        assert_eq!(run(f.wfc().args(["close", id]))["attempt"], attempt);
    }
    assert_eq!(
        run(f.wfc().args(["status", "--active"]))["dispatches"],
        json!([])
    );
    assert_eq!(run(f.wfc().arg("status"))["dispatches"][0]["dispatch"], id);
}

#[test]
#[cfg(target_os = "linux")]
fn missing_legacy_host_binding_cannot_confirm_close() {
    let f = Fixture::new();
    let dir = f.0.join("state/dispatches");
    fs::create_dir_all(&dir).unwrap();
    for backend in [None, Some(Value::Null), Some(json!(""))] {
        let id = "d-12345678";
        let mut record = json!({"dispatch":id,"round":1,"attempt":"a-abcdef012345","host":"codex"});
        if let Some(backend) = backend {
            record["backend"] = backend;
        }
        fs::write(dir.join(format!("{id}.json")), record.to_string()).unwrap();
        let cmd = || {
            let mut c = f.wfc();
            c.env("WFC_TMUX_SOCKET", f.0.join("missing.sock"));
            c
        };
        let query = run(cmd().args(["status", "--dispatch", id]));
        assert_eq!(query["dispatches"][0]["backend"], "tmux");
        assert_eq!(query["dispatches"][0]["status"], "unknown");
        assert!(!cmd().args(["close", id]).status().unwrap().success());
        assert!(fs::read_dir(&dir).unwrap().count() == 1);
    }
}

#[test]
fn invalid_public_options_and_launch_config_leave_no_active_dispatch() {
    let f = Fixture::new();
    fs::write(f.0.join("task"), "work").unwrap();
    fs::write(
        f.0.join("bad.json"),
        r#"{"hosts":{"claude":{"runtime":"bad secret value"}}}"#,
    )
    .unwrap();
    let args = [
        "dispatch",
        "--task",
        "task",
        "--cwd",
        ".",
        "--report",
        "new/report",
        "--host",
        "codex",
    ];
    for (extra, code, diagnostic) in [
        (vec!["--backend", "bundled"], 1, "unavailable"),
        (
            vec!["--herdr-endpoint", "socket"],
            2,
            "requires --backend herdr",
        ),
        (vec!["--scope", "--git"], 2, "missing value"),
        (vec!["--backend", "missing"], 2, "invalid --backend"),
    ] {
        let out = f.wfc().args(args).args(extra).output().unwrap();
        assert_eq!(out.status.code(), Some(code));
        assert!(String::from_utf8_lossy(&out.stderr).contains(diagnostic));
        assert!(out.stdout.is_empty());
    }
    #[cfg(target_os = "linux")]
    {
        let out = f
            .wfc()
            .args(args)
            .args(["--launch-config", "bad.json"])
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1));
        assert!(error.contains("invalid launch config") && !error.contains("secret value"));
    }
    assert_eq!(
        run(f.wfc().args(["status", "--active"]))["dispatches"],
        json!([])
    );
    assert!(fs::read_dir(f.0.join("state/dispatches"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
#[cfg(target_os = "linux")]
fn old_host_archive_preserves_known_exit_but_rechecks_session_cleanup() {
    let f = Fixture::new();
    let dir = f.0.join("state/dispatches");
    fs::create_dir_all(&dir).unwrap();
    let id = "d-abc01234";
    let attempt = "a-abcdef123456";
    let row = json!({"dispatch":id,"attempt":attempt,"round":1,"host":"codex","backend":"tmux",
        "tmuxSocket": f.0.join("offline.sock")});
    fs::write(dir.join(format!("{id}.json")), row.to_string()).unwrap();
    let path = dir.join(format!("{id}.{attempt}.closed"));
    for worker in [
        json!({"state":"exited","exit_code":37,"signal":null}),
        json!({"state":"cancelled","exit_code":null,"signal":null}),
    ] {
        let saved = json!({"dispatch":id,"attempt":attempt,"round":1,"worker":worker}).to_string();
        fs::write(&path, &saved).unwrap();
        let status = run(f.wfc().args(["status", "--dispatch", id]));
        let current = &status["dispatches"][0];
        assert_eq!(current["backendResult"], "uncertain");
        if worker["state"] == "exited" {
            assert_eq!(current["worker"], worker);
        } else {
            assert_eq!(current["worker"]["state"], "unknown");
        }
        let close = f.wfc().args(["close", id]).output().unwrap();
        assert_eq!(close.status.code(), Some(1));
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    }
}

#[test]
fn unresolved_reservations_stay_visible_without_claiming_other_assignments() {
    let f = Fixture::new();
    let dir = f.0.join("state/dispatches");
    fs::create_dir_all(&dir).unwrap();
    let id = "d-ffeeddcc";
    fs::write(dir.join(format!("{id}.pending")), b"{").unwrap();
    fs::write(dir.join("d-12345678.reserving"), b"{").unwrap();
    let all = run(f.wfc().args(["status", "--active"]));
    let rows = all["dispatches"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["dispatch"], id);
    assert_eq!(rows[0]["worker"]["state"], "unknown");
    assert_eq!(rows[0]["rounds"], json!([]));
    let named = run(f.wfc().args(["status", "--dispatch", id]));
    assert_eq!(named["dispatches"][0]["createPending"], true);
    #[cfg(target_os = "linux")]
    assert!(!f
        .wfc()
        .args(["close", id])
        .output()
        .unwrap()
        .status
        .success());
    let published = json!({"dispatch":id,"round":1,"attempt":"a-aaaabbbbcccc"});
    fs::write(dir.join(format!("{id}.json")), published.to_string()).unwrap();
    let all = run(f.wfc().arg("status"));
    assert_eq!(all["dispatches"].as_array().unwrap().len(), 1);
    assert!(all["dispatches"][0].get("createPending").is_none());
    assert_eq!(all["dispatches"][0]["attempt"], published["attempt"]);
}
