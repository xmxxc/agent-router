#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn empty_previews_leave_the_state_directory_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("never-created");
    for args in [vec!["status"], vec!["status", "--active"], vec!["reclaim"]] {
        let result = Command::new(env!("CARGO_BIN_EXE_wfc-legacy-state"))
            .args(args)
            .env("WFC_STATE", &state)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result.stderr);
        let view: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(view["state"], state.to_str().unwrap());
        assert!(!state.exists());
    }
}

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

fn command(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wfc-legacy-state"));
    command.env("WFC_STATE", state);
    command
}

fn json(command: &mut Command) -> Value {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn legacy(dir: &Path) {
    let db = Connection::open(dir.join("router.db")).unwrap();
    db.execute_batch(r#"CREATE TABLE dispatch (id TEXT PRIMARY KEY,host TEXT,cwd TEXT,task TEXT,created REAL,report_dir TEXT,report TEXT);
        CREATE TABLE message (id INTEGER PRIMARY KEY AUTOINCREMENT,dispatch TEXT,recipient TEXT,type TEXT,body TEXT,
            created REAL,delivered INTEGER DEFAULT 0,state TEXT,status_version INTEGER);
        INSERT INTO dispatch VALUES('old','codex','/work','/tasks/one',1,'/reports/old-reviewer','/reports/r1');
        INSERT INTO message VALUES (42,'old','controller','done','{ "report" : "/reports/r1", "text":"原文" }',8,1,'unsent',5),
            (43,'old','controller','question','{}',9,0,'unknown',4),
            (44,'old','controller','question','{}',10,1,'processed',3),
            (46,'old','controller','question','{}',11,0,'received',2),
            (48,'old','controller','question','{}',12,1,NULL,NULL);"#).unwrap();
}

fn details(dir: &Path) -> Value {
    json(command(dir).args(["status", "--dispatch", "old"]))
}

#[test]
fn old_queries_leave_schema_and_original_bytes_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("router.db");
    legacy(tmp.path());
    let before = Sha256::digest(fs::read(&path).unwrap());
    let view = details(tmp.path());
    assert!(view["schema"].is_string());
    let dispatch = &view["dispatches"][0];
    assert!(dispatch["attempt"].is_null());
    assert!(dispatch["backendAlive"].is_null());
    assert_eq!(dispatch["messages"][0]["messageId"], "42");
    assert_eq!(dispatch["messages"][0]["text"], "原文");
    let reclaim = json(command(tmp.path()).args(["reclaim", "--dispatch", "old"]));
    assert_eq!(reclaim["sessions"][0]["result"], "unknown");
    assert_eq!(Sha256::digest(fs::read(path).unwrap()), before);
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
}

#[test]
fn concurrent_migrations_publish_one_backup_and_keep_receipts_and_history_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy(dir);
    let children: Vec<_> = (0..8)
        .map(|_| {
            command(dir)
                .args(["migrate", "--offline-copy"])
                .arg(dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut backups = vec![];
    for child in children {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        if let Some(path) = result["backup"].as_str() {
            backups.push(path.to_owned());
        }
    }
    assert_eq!(backups.len(), 1);
    let before =
        Connection::open_with_flags(&backups[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        before
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='dispatch_round'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        before
            .query_row("SELECT status_version FROM message WHERE id=42", [], |r| {
                r.get::<_, usize>(0)
            })
            .unwrap(),
        5
    );
    let view = details(dir);
    assert!(view.get("schema").is_none());
    let d = &view["dispatches"][0];
    assert_eq!(d["role"], "reviewer");
    assert!(d["scope"].as_str().unwrap().starts_with("s-"));
    assert!(d["attempt"].as_str().unwrap().starts_with("a-"));
    let r = &d["rounds"][0];
    assert_eq!(r["attempt"], d["attempt"]);
    assert_eq!(r["state"], "completed");
    assert_eq!(r["completed"], 8.0);
    assert_eq!(r["report"], "/reports/r1");
    assert_eq!(r["task"], "/tasks/one");
    let msgs = d["messages"].as_array().unwrap();
    for (m, (id, state, version)) in msgs.iter().zip([
        (42, "received", 6),
        (43, "unknown", 4),
        (44, "processed", 3),
        (46, "unsent", 3),
        (48, "received", 1),
    ]) {
        assert_eq!(m["id"], id);
        assert_eq!(m["state"], state);
        assert_eq!(m["statusVersion"], version);
        assert!(m["messageId"].as_str().unwrap().starts_with("m-"));
    }
    assert_eq!(r["result"], msgs[0]["body"]);
    assert_eq!(
        msgs[0]["bodyHash"],
        format!(
            "{:x}",
            Sha256::digest(r#"{ "report" : "/reports/r1", "text":"原文" }"#.as_bytes())
        )
    );
    assert!(json(command(dir).args(["migrate", "--offline-copy"]).arg(dir))["backup"].is_null());
    assert_eq!(details(dir), view);
}

#[test]
fn killed_migration_rolls_back_then_restarts_from_original_receipts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy(dir);
    // A real SQLite reader allows BEGIN IMMEDIATE and writes to the rollback journal,
    // but keeps the migrator from committing until we release it.
    let reader = Connection::open(dir.join("router.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM message;")
        .unwrap();
    let mut child = command(dir)
        .args(["migrate", "--offline-copy"])
        .arg(dir)
        .spawn()
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(10);
    while !dir.join("router.db-journal").exists() && Instant::now() < limit {
        thread::sleep(Duration::from_millis(5));
    }
    let interrupted = dir.join("router.db-journal").exists();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(interrupted);
    drop(reader);
    let path = dir.join("router.db");
    let recovered = Connection::open(&path).unwrap();
    assert_eq!(
        recovered
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='dispatch_round'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        0
    );
    drop(recovered);
    json(command(dir).args(["migrate", "--offline-copy"]).arg(dir));
    let d = details(dir);
    assert_eq!(d["dispatches"][0]["messages"][0]["statusVersion"], 6);
    assert_eq!(d["dispatches"][0]["rounds"][0]["state"], "completed");
    json(command(dir).args(["migrate", "--offline-copy"]).arg(dir));
    assert_eq!(details(dir), d);
}

#[test]
fn discovers_python_default_relative_and_tilde_directories() {
    let tmp = tempfile::tempdir().unwrap();
    for (value, suffix) in [
        (None, ".local/state/workflow-core"),
        (Some(""), ".local/state/workflow-core"),
        (Some("~/copy"), "copy"),
        (Some("local/../copy"), "copy"),
    ] {
        let mut cmd = command(tmp.path());
        cmd.env("HOME", tmp.path())
            .env_remove("WFC_STATE")
            .current_dir(tmp.path())
            .arg("status");
        if let Some(s) = value {
            cmd.env("WFC_STATE", s);
        }
        assert_eq!(
            json(&mut cmd)["state"],
            tmp.path().join(suffix).to_str().unwrap()
        );
    }
}

#[test]
fn reused_report_does_not_finish_another_attempt_and_runtime_identities_remain_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (db, _) = wfc_legacy_state::open_offline_copy(dir).unwrap();
    db.execute_batch(r#"INSERT INTO dispatch(id,backend,scope,role,current_round,current_attempt,report,backend_handle)
        VALUES('old','herdr','scope-x','worker',2,'a-current','/r','{"workspace":"w", "pane":"p", "terminal":"t"}');
        INSERT INTO dispatch_round(dispatch,round,attempt,state,report) VALUES
            ('old',2,'a-before','unknown','/r'),('old',2,'a-current','running','/r');
        INSERT INTO message(id,dispatch,type,body) VALUES
            (1,'old','done','{"report":"/r"}'),
            (2,'old','done','{"report":"/r","round":2,"attempt":"a-before"}');
        INSERT INTO waiter(pid,kind,since,process_start) VALUES(123,'relay',1,'old-boot/start');
        INSERT INTO lifecycle_event(dispatch,operation,result,owner_token,resolver_actor,resolution_evidence)
            VALUES('old','handoff','unknown','private-test-token','fixture-actor','evidence-1');
        "#).unwrap();
    drop(db);
    json(command(dir).args(["migrate", "--offline-copy"]).arg(dir));
    let view = details(dir);
    let d = &view["dispatches"][0];
    assert_eq!(d["status"], "running");
    assert_eq!(d["backendHandle"]["pane"], "p");
    assert!(d["backendAlive"].is_null());
    assert_eq!(d["audit"][0]["resolutionEvidence"], "evidence-1");
    assert!(!view.to_string().contains("private-test-token"));
    assert_eq!(view["waiters"][0]["processStart"], "old-boot/start");
    assert_eq!(view["waiters"][0]["liveness"], "unknown");
    assert_eq!(d["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn wal_preview_gives_recovery_guidance_without_making_sidecars() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy(dir);
    let db = Connection::open(dir.join("router.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    drop(db);
    let before = fs::read(dir.join("router.db")).unwrap();
    let result = command(dir).arg("status").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("journal_mode=DELETE"));
    assert_eq!(fs::read(dir.join("router.db")).unwrap(), before);
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn migration_rejects_aliased_database_files_and_preserves_original() {
    let tmp = tempfile::tempdir().unwrap();
    legacy(tmp.path());
    let original = tmp.path().join("router.db");
    let before = fs::read(&original).unwrap();
    let copy = tmp.path().join("copy");
    fs::create_dir(&copy).unwrap();
    for symlink in [true, false] {
        let link = copy.join("router.db");
        if symlink {
            std::os::unix::fs::symlink(&original, &link).unwrap();
        } else {
            fs::hard_link(&original, &link).unwrap();
        }
        let result = command(&copy)
            .args(["migrate", "--offline-copy"])
            .arg(&copy)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("independent"));
        assert_eq!(fs::read(&original).unwrap(), before);
        fs::remove_file(link).unwrap();
    }
}

#[test]
fn evidence_without_dispatch_rows_is_visible_before_and_after_migration() {
    let original = tempfile::tempdir().unwrap();
    let copy = tempfile::tempdir().unwrap();
    let db_file = original.path().join("router.db");
    let db = Connection::open(&db_file).unwrap();
    db.execute_batch(r#"
        CREATE TABLE fault_event(dispatch TEXT PRIMARY KEY,operation TEXT,diagnostic TEXT);
        INSERT INTO fault_event VALUES('fault-only','create','writer unconfirmed');
        CREATE TABLE message(id INTEGER PRIMARY KEY,dispatch TEXT,recipient TEXT,type TEXT,body TEXT,delivered INTEGER);
        INSERT INTO message VALUES(41,'receipt-only','controller','done','{"report":"/old/r"}',1);
        CREATE TABLE dispatch_round(dispatch TEXT,round INTEGER,attempt TEXT,state TEXT,report TEXT,
            PRIMARY KEY(dispatch,round,attempt));
        INSERT INTO dispatch_round VALUES('attempt-only',2,'a-old','completed','/old/round-two');
        CREATE TABLE watchdog_fault(dispatch TEXT,round INTEGER,attempt TEXT,kind TEXT,fingerprint TEXT,
            PRIMARY KEY(dispatch, round, attempt, kind, fingerprint));
        INSERT INTO watchdog_fault VALUES('watchdog-only',2,'a-old','timeout','deadline');
        CREATE TABLE host_error(dispatch TEXT PRIMARY KEY,turn TEXT);
        INSERT INTO host_error VALUES('host-error-only','old-turn');
        CREATE TABLE lifecycle_event(id INTEGER PRIMARY KEY,dispatch TEXT,operation TEXT,attempt TEXT);
        INSERT INTO lifecycle_event VALUES(19,'audit-only','create','a-pending');"#).unwrap();
    drop(db);
    let original_hash = Sha256::digest(fs::read(&db_file).unwrap());
    fs::copy(&db_file, copy.path().join("router.db")).unwrap();

    for dir in [original.path(), copy.path()] {
        if dir == copy.path() {
            let (upgraded, backup) = wfc_legacy_state::open_offline_copy(dir).unwrap();
            assert!(backup.unwrap().is_file());
            assert_eq!(
                upgraded
                    .query_row("SELECT count(*) FROM dispatch", [], |r| r
                        .get::<_, usize>(0))
                    .unwrap(),
                0
            );
        }
        let overview = json(command(dir).arg("status"));
        let rows = overview["dispatches"].as_array().unwrap();
        assert_eq!(rows.len(), 6);
        for entry in rows {
            for key in [
                "attempt",
                "round",
                "host",
                "backend",
                "scope",
                "task",
                "report",
                "backendAlive",
            ] {
                assert_eq!(entry.get(key), Some(&Value::Null), "{key}");
            }
            assert_eq!(entry["status"], "unknown");
            assert!(entry["diagnostic"]
                .as_str()
                .unwrap()
                .contains("dispatch row"));
            let id = entry["dispatch"].as_str().unwrap();
            let detail = json(command(dir).args(["status", "--dispatch", id]));
            assert_eq!(detail["dispatches"][0], *entry);
            let session = &json(command(dir).args(["reclaim", "--dispatch", id]))["sessions"][0];
            assert_eq!(session["result"], "unknown");
            assert!(session["backend"].is_null());
            match id {
                "fault-only" => assert_eq!(entry["fault"]["diagnostic"], "writer unconfirmed"),
                "receipt-only" => {
                    assert_eq!(entry["messages"][0]["id"], 41);
                    assert_eq!(entry["messages"][0]["state"], "received");
                    assert_eq!(entry["messages"][0]["report"], "/old/r");
                }
                "attempt-only" => assert_eq!(entry["rounds"][0]["attempt"], "a-old"),
                "watchdog-only" => {
                    assert_eq!(entry["watchdogFaults"][0]["kind"], "timeout");
                    assert_eq!(entry["watchdogFaults"][0]["attempt"], "a-old");
                }
                "host-error-only" => assert_eq!(entry["hostError"]["turn"], "old-turn"),
                "audit-only" => assert_eq!(entry["audit"][0]["attempt"], "a-pending"),
                _ => panic!("unexpected dispatch {id}"),
            }
        }
        assert!(overview["undelivered"].as_array().unwrap().is_empty());
    }
    assert_eq!(Sha256::digest(fs::read(db_file).unwrap()), original_hash);
}

#[test]
fn conflicting_current_snapshot_refuses_fast_and_repair_paths() {
    for pending_repair in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.db");
        let (db, _) = wfc_legacy_state::open_offline_copy(dir.path()).unwrap();
        db.execute_batch(
            "INSERT INTO dispatch(id,backend,scope,role,current_round,current_attempt)
            VALUES('old','tmux','s','worker',2,'a-current');
            INSERT INTO dispatch_round(dispatch,round,attempt,state) VALUES
            ('old',1,'a-current','completed'),('old',2,'a-previous','running');",
        )
        .unwrap();
        if pending_repair {
            db.execute("INSERT INTO message(body) VALUES('{}')", [])
                .unwrap();
        }
        drop(db);
        let before = Sha256::digest(fs::read(&path).unwrap());
        let err = wfc_legacy_state::open_offline_copy(dir.path())
            .unwrap_err()
            .to_string();
        for clue in ["old", "2", "a-current", "snapshot"] {
            assert!(err.contains(clue), "{err}");
        }
        let view = details(dir.path());
        assert_eq!(view["dispatches"][0]["status"], "unknown");
        assert!(view["dispatches"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("snapshot"));
        assert_eq!(Sha256::digest(fs::read(path).unwrap()), before);
    }
}

#[test]
fn resolves_dangling_state_links_and_rejects_unknown_named_home() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::os::unix::fs::symlink("missing/target", dir.join("alias")).unwrap();
    std::os::unix::fs::symlink("alias/../state", dir.join("chain")).unwrap();
    let view = json(
        command(dir)
            .env("WFC_STATE", dir.join("chain/router"))
            .arg("status"),
    );
    assert_eq!(
        view["state"],
        dir.join("missing/state/router").to_str().unwrap()
    );
    assert!(!dir.join("missing").exists());
    let result = command(dir)
        .env("WFC_STATE", "~wfc-no-such-user-3ef7f43c/state")
        .current_dir(dir)
        .arg("status")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("home directory"));
}
