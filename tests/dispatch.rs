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
fn live_worker_reports_once_and_wait_returns_the_same_receipt() {
    let f = Fixture::new();
    let worker = r#"
import json, os, subprocess, sys
from pathlib import Path
Path(os.environ["WFC_REPORT"]).write_text("finished")
args = [sys.argv[1], "done", "--report", os.environ["WFC_REPORT"], "--attempt", os.environ["WFC_ATTEMPT"]]
first = json.loads(subprocess.check_output(args))
assert first["state"] == "unsent" and not first["duplicate"]
"#;
    let record = run(f.dispatch("work", "reviewer", "task.md").args([
        PYTHON,
        "-c",
        worker,
        env!("CARGO_BIN_EXE_wfc"),
    ]));
    let batch = run(f.wfc().args(["wait", "--timeout", "5"]));
    let receipt = run(f
        .wfc()
        .env("WFC_DISPATCH", record["dispatch"].as_str().unwrap())
        .env("WFC_ROUND", "1")
        .env("WFC_ATTEMPT", record["attempt"].as_str().unwrap())
        .args([
            "done",
            "--report",
            record["report"].as_str().unwrap(),
            "--attempt",
            record["attempt"].as_str().unwrap(),
        ]));
    assert_eq!(receipt["duplicate"], true);
    let messages = batch["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    for field in ["dispatch", "round", "attempt", "role"] {
        assert_eq!(message[field], record[field]);
    }
    assert_eq!(message["report"], record["report"]);
    assert_eq!(message["messageId"], receipt["messageId"]);
    assert_eq!(message["id"], receipt["message"]);
    assert_eq!(message["state"], "received");
    assert_eq!(message["statusVersion"], 2);
    let history = run(f
        .wfc()
        .args(["status", "--dispatch", record["dispatch"].as_str().unwrap()]));
    let stored = &history["dispatches"][0]["messages"][0];
    for (key, value) in message.as_object().unwrap() {
        assert_eq!(stored[key], *value);
    }
    assert_eq!(stored["delivered"], true);
    let timeout = f.wfc().args(["wait", "--timeout", "0"]).output().unwrap();
    assert_eq!(timeout.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<Value>(&timeout.stdout).unwrap(),
        json!({"messages": [], "timedOut": true})
    );
}

#[test]
fn invalid_callbacks_leave_the_queue_empty_and_parallel_retries_share_one_receipt() {
    let f = Fixture::new();
    let record = run(f
        .dispatch("work", "reviewer", "task.md")
        .args([PYTHON, "-c", "pass"]));
    let report = record["report"].as_str().unwrap();
    fs::write(report, "done").unwrap();
    let id = record["dispatch"].as_str().unwrap();
    let done = |report: &str| {
        let mut cmd = f.wfc();
        cmd.env("WFC_DISPATCH", id)
            .env("WFC_ROUND", "1")
            .env("WFC_ATTEMPT", record["attempt"].as_str().unwrap())
            .args(["done", "--report", report, "--message-id", "review"]);
        cmd
    };
    for mut cmd in [
        done("absent.md"),
        {
            let mut c = done(report);
            c.env("WFC_ROUND", "2");
            c
        },
        {
            let mut c = done(report);
            c.env("WFC_ATTEMPT", "stale");
            c
        },
        {
            let mut c = done(report);
            c.args(["--attempt", "stale"]);
            c
        },
    ] {
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
    assert_eq!(
        f.wfc()
            .args(["wait", "--timeout", "0"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(3)
    );
    let (a, b) = std::thread::scope(|scope| {
        let other = scope.spawn(|| run(&mut done(report)));
        (run(&mut done(report)), other.join().unwrap())
    });
    assert_eq!(a["messageId"], b["messageId"]);
    assert_ne!(a["duplicate"], b["duplicate"]);
    fs::write(f.0.join("second.md"), "different report").unwrap();
    let conflict = done("second.md").output().unwrap();
    assert_eq!(conflict.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("conflicts"));
    let messages = run(f.wfc().args(["wait", "--timeout", "0"]));
    assert_eq!(messages["messages"].as_array().unwrap().len(), 1);
    let replay = run(&mut done(report));
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["state"], "received");
    assert_eq!(replay["statusVersion"], 2);
    assert_eq!(replay["messageId"], a["messageId"]);
}

#[test]
fn controller_receipts_survive_restarts_and_concurrent_confirmation() {
    let status = Command::new(PYTHON)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/receipts.py"))
        .arg(env!("CARGO_BIN_EXE_wfc"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn bound_report_unkeyed_callbacks_and_scope_follow_the_assignment() {
    let f = Fixture::new();
    fs::write(f.0.join("task"), "").unwrap();
    let record = run(f.wfc().args([
        "dispatch",
        "--role",
        "worker",
        "--task",
        "task",
        "--cwd",
        ".",
        "--report",
        "reports/current.md",
        "--scope",
        "team",
        "--",
        PYTHON,
        "-c",
        "pass",
    ]));
    fs::write(f.0.join("reports/current.md"), "finished").unwrap();
    let callback = |path: &str| {
        let mut c = f.wfc();
        c.env_remove("WFC_ROUND")
            .env_remove("WFC_ATTEMPT")
            .env("WFC_DISPATCH", record["dispatch"].as_str().unwrap())
            .args(["done", "--report", path]);
        c
    };
    assert_eq!(
        callback("task")
            .args(["--message-id", "X"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    let a = run(&mut callback("reports/current.md"));
    let b = run(&mut callback("reports/current.md"));
    assert_ne!(a["messageId"], b["messageId"]);
    let mut ids = Vec::new();
    for key in ["X", "Y", "X"] {
        ids.push(
            run(callback("reports/current.md").args(["--message-id", key]))["messageId"].clone(),
        );
    }
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ids[0], ids[2]);
    let other = f
        .wfc()
        .args(["wait", "--scope", "other", "--timeout", "0"])
        .output()
        .unwrap();
    assert_eq!(other.status.code(), Some(3));
    let batch = run(f.wfc().args(["wait", "--scope", "team", "--timeout", "0"]));
    let msgs = batch["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert!(msgs.iter().all(|m| m["scope"] == "team"
        && m.get("head") == Some(&Value::Null)
        && m.get("dirty") == Some(&Value::Null)));
}

#[test]
fn git_snapshot_is_taken_in_worker_directory_and_preserved_on_keyed_retry() {
    let f = Fixture::new();
    let work = f.0.join("work");
    fs::create_dir(&work).unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git")
            .current_dir(&work)
            .args([
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["commit", "--allow-empty", "-qm", "first"]);
    let first_head = git(&["rev-parse", "HEAD"]);
    fs::write(f.0.join("task"), "").unwrap();
    let record = run(f.wfc().args([
        "dispatch",
        "--cwd",
        "work",
        "--role",
        "tester",
        "--task",
        "task",
        "--report",
        "out/current",
        "--",
        PYTHON,
        "-c",
        "pass",
    ]));
    fs::write(f.0.join("out/current"), "done").unwrap();
    let done = |key: &str| {
        run(f
            .wfc()
            .env("GIT_DIR", f.0.join("other.git"))
            .env("GIT_INDEX_FILE", f.0.join("other.index"))
            .env_remove("WFC_ROUND")
            .env_remove("WFC_ATTEMPT")
            .env("WFC_DISPATCH", record["dispatch"].as_str().unwrap())
            .args(["done", "--report", "out/current", "--message-id", key]))
    };
    let first = done("clean");
    git(&["commit", "--allow-empty", "-qm", "second"]);
    fs::write(work.join("untracked"), "").unwrap();
    assert_eq!(done("clean")["messageId"], first["messageId"]);
    done("dirty");
    let batch = run(f.wfc().args(["wait", "--timeout", "0"]));
    assert_eq!(batch["messages"][0]["head"], first_head);
    assert_eq!(batch["messages"][0]["dirty"], false);
    assert_eq!(batch["messages"][1]["head"], git(&["rev-parse", "HEAD"]));
    assert_eq!(batch["messages"][1]["dirty"], true);
}

#[cfg(unix)]
#[test]
fn failed_or_timed_out_git_leaves_both_snapshot_fields_null() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    let record = run(f
        .dispatch("work", "reviewer", "task")
        .args([PYTHON, "-c", "pass"]));
    let report = record["report"].as_str().unwrap();
    fs::write(report, "").unwrap();
    let git = f.0.join("git");
    let path = std::env::join_paths(
        std::iter::once(f.0.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let done = |key| {
        run(f
            .wfc()
            .env("PATH", &path)
            .env_remove("WFC_ROUND")
            .env_remove("WFC_ATTEMPT")
            .env("WFC_DISPATCH", record["dispatch"].as_str().unwrap())
            .args(["done", "--report", report, "--message-id", key]))
    };
    fs::write(&git, "#!/bin/sh\nexit 128\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    done("failed");
    // HEAD succeeds; status hangs. A partial snapshot cannot describe this tree.
    fs::write(&git, "#!/bin/sh\nif [ \"$1\" = rev-parse ]; then echo 0123456789abcdef0123456789abcdef01234567; else exec sleep 60; fi\n").unwrap();
    let started = Instant::now();
    let first = done("timeout");
    assert!(started.elapsed() < Duration::from_secs(15));
    let retry = Instant::now();
    assert_eq!(done("timeout")["messageId"], first["messageId"]);
    assert!(retry.elapsed() < Duration::from_secs(4));
    let batch = run(f.wfc().args(["wait", "--timeout", "0"]));
    let messages = batch["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages
        .iter()
        .all(|m| m.get("head") == Some(&Value::Null) && m.get("dirty") == Some(&Value::Null)));
}

#[test]
fn default_reports_in_shared_cwd_keep_each_scopes_contents() {
    let f = Fixture::new();
    fs::write(f.0.join("task"), "").unwrap();
    let worker = r#"
import os, subprocess, sys
from pathlib import Path
path = Path(os.environ['WFC_REPORT'])
path.write_text(os.environ['WFC_ROLE'])
subprocess.run([sys.argv[1], 'done', '--report', path, '--attempt', os.environ['WFC_ATTEMPT']], check=True)
"#;
    for scope in ["alpha", "beta"] {
        run(f.wfc().args([
            "dispatch",
            "--cwd",
            ".",
            "--task",
            "task",
            "--role",
            scope,
            "--scope",
            scope,
            "--",
            PYTHON,
            "-c",
            worker,
            env!("CARGO_BIN_EXE_wfc"),
        ]));
    }
    for scope in ["alpha", "beta"] {
        let batch = run(f.wfc().args(["wait", "--scope", scope, "--timeout", "5"]));
        let msg = &batch["messages"][0];
        assert_eq!(
            fs::read_to_string(msg["report"].as_str().unwrap()).unwrap(),
            scope
        );
    }
}

#[test]
#[cfg(unix)]
fn explicit_report_link_can_point_to_a_future_file() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    fs::write(f.0.join("task"), "").unwrap();
    symlink("new/reports/result.md", f.0.join("report-link")).unwrap();
    let request_path = "missing/../report-link";
    let write_report = "import os; from pathlib import Path; Path(os.environ['WFC_REPORT']).write_text('finished')";
    let record = run(f.wfc().args([
        "dispatch",
        "--cwd",
        ".",
        "--task",
        "task",
        "--role",
        "tester",
        "--report",
        request_path,
        "--",
        PYTHON,
        "-c",
        write_report,
    ]));
    let record = completed(&f, record, 0);
    let target = f.0.join("new/reports/result.md").canonicalize().unwrap();
    assert_eq!(record["report"], target.to_str().unwrap());
    assert_eq!(
        fs::read_to_string(f.0.join("report-link")).unwrap(),
        "finished"
    );
    run(f
        .wfc()
        .env("WFC_DISPATCH", record["dispatch"].as_str().unwrap())
        .env_remove("WFC_ROUND")
        .env_remove("WFC_ATTEMPT")
        .args(["done", "--report", request_path]));
}

#[test]
fn r1_bare_retry_preserves_receipt_across_upgrade() {
    let f = Fixture::new();
    let dispatch = "d-00000000000000000000000000000001";
    let attempt = "a-001";
    let root = f.0.canonicalize().unwrap();
    let state = root.join("state");
    fs::create_dir_all(state.join("dispatches")).unwrap();
    fs::write(root.join("custom"), "finished").unwrap();
    let record = json!({"dispatch": dispatch, "attempt": attempt, "round": 1, "role": "tester", "cwd": root});
    fs::write(
        state.join("dispatches").join(format!("{dispatch}.json")),
        record.to_string(),
    )
    .unwrap();
    // On disk, R1 stored its environment-derived key even for a bare done.
    let mut prior = record;
    prior.as_object_mut().unwrap().remove("cwd");
    prior["id"] = json!(1);
    prior["messageId"] = json!("m-original");
    prior["report"] = json!(root.join("custom"));
    prior["request_key"] = json!(format!("done:{dispatch}:{attempt}"));
    prior["received"] = json!(false);
    fs::write(state.join("messages.json"), json!([prior]).to_string()).unwrap();
    for (expected_state, version) in [("unsent", 1), ("received", 2)] {
        let receipt = run(f
            .wfc()
            .env("WFC_DISPATCH", dispatch)
            .env("WFC_ROUND", "1")
            .env("WFC_ATTEMPT", attempt)
            .args(["done", "--report", "custom"]));
        assert_eq!(receipt["message"], 1);
        assert_eq!(receipt["messageId"], "m-original");
        assert_eq!(receipt["duplicate"], true);
        assert_eq!(receipt["state"], expected_state);
        assert_eq!(receipt["statusVersion"], version);
        let wait = f.wfc().args(["wait", "--timeout", "0"]).output().unwrap();
        let batch: Value = serde_json::from_slice(&wait.stdout).unwrap();
        if expected_state == "unsent" {
            assert!(wait.status.success());
            assert_eq!(batch["messages"].as_array().unwrap().len(), 1);
            assert_eq!(batch["messages"][0]["messageId"], "m-original");
        } else {
            assert_eq!(wait.status.code(), Some(3));
            assert_eq!(batch, json!({"messages":[],"timedOut":true}));
        }
    }
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

#[test]
#[cfg(target_os = "linux")]
fn codex_relay_contracts() {
    let status = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/relay.py"))
        .arg(env!("CARGO_BIN_EXE_wfc"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[cfg(target_os = "linux")]
fn opencode_relay_contracts() {
    let status = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/opencode_relay.py"
        ))
        .arg(env!("CARGO_BIN_EXE_wfc"))
        .status()
        .unwrap();
    assert!(status.success());
}
