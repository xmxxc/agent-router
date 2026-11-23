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
    let all = run(f.wfc().arg("status"));
    let rows = all["dispatches"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (record, dir, task, role) in [
        (a, "left", "first.md", "writer"),
        (b, "right", "second.md", "reader"),
    ] {
        assert!(rows.contains(&record));
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
fn failed_handoffs_leave_only_the_successful_dispatch_queryable() {
    let f = Fixture::new();
    assert_eq!(run(f.wfc().arg("status")), json!({"dispatches": []}));
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
    for error in [missing, failed] {
        assert!(!error.status.success());
        assert!(error.stdout.is_empty());
        assert!(!error.stderr.is_empty());
    }
    assert_eq!(run(f.wfc().arg("status")), json!({"dispatches": [good]}));
    let unknown = f
        .wfc()
        .args(["status", "--dispatch", "d-00000000000000000000000000000000"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
}
