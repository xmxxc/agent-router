use super::*;
use crate::workflow::host::{Launch, Options};
use uuid::Uuid;

const ATTEMPT: &str = "a-11111111111111111111111111111111";
const NEXT_ATTEMPT: &str = "a-22222222222222222222222222222222";

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    config: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!("wfc-opencode-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let cwd = root.join("checkout with space");
        fs::create_dir(&cwd).unwrap();
        let config = root.join("launch.json");
        let store = root.join("worker.db");
        let fixture = Self {
            root,
            cwd,
            config,
            store,
        };
        fixture.set_store(&fixture.store);
        fixture
    }

    fn set_store(&self, store: &Path) {
        fs::write(
            &self.config,
            json!({"hosts":{"opencode":{
                "command":["opencode-fixture"],"env":{"OPENCODE_DB":store,"WFC_OPENCODE_DB":store}
            }}})
            .to_string(),
        )
        .unwrap();
    }

    fn launch(&self) -> (Value, String) {
        let mut record = json!({
            "dispatch":"d-33333333333333333333333333333333", "attempt": ATTEMPT, "round":1,
            "cwd":self.cwd, "scope":"fixture", "task":self.root.join("task 合成.md"),
            "role":"fixture", "report":self.root.join("report.md"), "host":"opencode", "backend":"tmux",
            "model":"fixture/provider"
        });
        let launch = Options {
            name: "opencode",
            backend: "tmux",
            config: self.config.to_str(),
            model: Some("fixture/provider"),
            effort: Some("high"),
            ..Options::default()
        }
        .prepare(&self.root, &mut record)
        .unwrap();
        let brief = launch.argv.last().unwrap().to_owned();
        assert!(brief.ends_with(&format!("Current attempt {ATTEMPT}.")));
        assert_eq!(
            launch.argv,
            [
                "opencode-fixture",
                self.cwd.to_str().unwrap(),
                "-m",
                "fixture/provider",
                "--variant",
                "high",
                "--prompt",
                &brief
            ]
        );
        assert_eq!(launch.env["OPENCODE_DB"], json!(self.store));
        assert_eq!(record["opencodeOpening"]["attempt"], ATTEMPT);
        assert!(!record.to_string().contains(&brief));
        assert!(record["hostSession"].is_null());
        (record, brief)
    }

    fn resume(&self, original: &Value) -> Result<(Launch, Value)> {
        let mut next = original.clone();
        next["attempt"] = json!(NEXT_ATTEMPT);
        next["round"] = json!(2);
        let launch = Options::prepare_opencode_resume(&self.root, original, &mut next)?;
        Ok((launch, next))
    }

    fn db(&self) -> Connection {
        let db = Connection::open(&self.store).unwrap();
        db.execute_batch("CREATE TABLE project(id TEXT PRIMARY KEY,worktree TEXT NOT NULL);")
            .unwrap();
        db.execute(
            "INSERT INTO project VALUES('project',?1)",
            [self.cwd.to_str().unwrap()],
        )
        .unwrap();
        db
    }

    fn assert_resume(&self, original: &Value, id: &str, message: &str) -> Value {
        let before = fs::read(&self.store).unwrap();
        let (launch, next) = self.resume(original).unwrap();
        assert_eq!(
            launch.argv,
            [
                "opencode-fixture",
                self.cwd.to_str().unwrap(),
                "-m",
                "fixture/provider",
                "--variant",
                "high",
                "-s",
                id
            ]
        );
        assert_eq!(launch.env["WFC_ATTEMPT"], NEXT_ATTEMPT);
        assert_eq!(launch.env["WFC_ROUND"], "2");
        assert_eq!(next["hostSession"], id);
        assert_eq!(
            next["opencodeSession"],
            json!({"project":"project","firstMessage":message})
        );
        assert_eq!(next["opencodeOpening"], original["opencodeOpening"]);
        assert_eq!(fs::read(&self.store).unwrap(), before);
        next
    }

    fn assert_rejected(&self, original: &Value, text: &str) {
        match self.resume(original) {
            Ok(_) => panic!("expected OpenCode refusal containing {text}"),
            Err(error) => assert!(format!("{error:#}").contains(text), "{error:#}"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn legacy(db: &Connection) {
    db.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY,project_id TEXT,directory TEXT,parent_id TEXT);
        CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
        CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,data TEXT);",
    )
    .unwrap();
}

fn modern(db: &Connection, table: &str) {
    if table == "session_v2" {
        db.execute_batch(
            "CREATE TABLE session_v2(id TEXT PRIMARY KEY,project_id TEXT,directory TEXT,
            parent_id TEXT,fork_session_id TEXT);",
        )
        .unwrap();
    }
    db.execute_batch(&format!(
        "CREATE TABLE session_message(id TEXT PRIMARY KEY,
        session_id TEXT REFERENCES {table}(id),type TEXT,seq INTEGER,data TEXT);"
    ))
    .unwrap();
}

fn legacy_message(
    db: &Connection,
    sid: &str,
    mid: &str,
    order: u32,
    role: &str,
    text: &str,
    synthetic: bool,
) {
    db.execute(
        "INSERT INTO message VALUES(?1,?2,?3,?4)",
        params![mid, sid, order, json!({"role":role}).to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO part VALUES(?1,?1,?2,?3)",
        params![
            mid,
            sid,
            json!({"type":"text","text":text,"synthetic":synthetic}).to_string()
        ],
    )
    .unwrap();
}

fn modern_message(db: &Connection, sid: &str, mid: &str, order: u32, kind: &str, text: &str) {
    db.execute(
        "INSERT INTO session_message VALUES(?1,?2,?3,?4,?5)",
        params![mid, sid, kind, order, json!({"text":text}).to_string()],
    )
    .unwrap();
}

#[test]
fn legacy_first_real_user_opening_and_explicit_argv() {
    let fixture = Fixture::new();
    let (record, brief) = fixture.launch();
    let db = fixture.db();
    legacy(&db);
    db.execute_batch("INSERT INTO project VALUES('other','/elsewhere');")
        .unwrap();
    for (id, cwd, parent) in [
        ("ses_original", fixture.cwd.as_path(), None),
        ("ses_echo", fixture.cwd.as_path(), None),
        ("ses_later", fixture.cwd.as_path(), None),
        ("ses_synthetic", fixture.cwd.as_path(), None),
        ("ses_child", fixture.cwd.as_path(), Some("ses_original")),
        ("ses_wrong_cwd", fixture.root.as_path(), None),
        ("ses_wrong_part", fixture.cwd.as_path(), None),
    ] {
        db.execute(
            "INSERT INTO session VALUES(?1,'project',?2,?3)",
            params![id, cwd.to_str().unwrap(), parent],
        )
        .unwrap();
    }
    legacy_message(
        &db,
        "ses_original",
        "msg_original",
        2,
        "user",
        &brief,
        false,
    );
    legacy_message(&db, "ses_echo", "msg_echo", 1, "assistant", &brief, false);
    legacy_message(&db, "ses_later", "msg_early", 1, "user", "else", false);
    legacy_message(&db, "ses_later", "msg_later", 2, "user", &brief, false);
    legacy_message(
        &db,
        "ses_synthetic",
        "msg_synthetic",
        1,
        "user",
        &brief,
        true,
    );
    legacy_message(&db, "ses_child", "msg_child", 1, "user", &brief, false);
    legacy_message(
        &db,
        "ses_wrong_cwd",
        "msg_wrong_cwd",
        1,
        "user",
        &brief,
        false,
    );
    legacy_message(
        &db,
        "ses_wrong_part",
        "msg_wrong_part",
        1,
        "user",
        &brief,
        false,
    );
    db.execute(
        "UPDATE part SET session_id='ses_echo' WHERE message_id='msg_wrong_part'",
        [],
    )
    .unwrap();
    let saved = fixture.assert_resume(&record, "ses_original", "msg_original");
    let mut recheck = saved.clone();
    recheck["attempt"] = json!("a-44444444444444444444444444444444");
    let launch = Options::prepare_opencode_resume(&fixture.root, &saved, &mut recheck).unwrap();
    assert_eq!(launch.argv.last().unwrap(), "ses_original");
    db.execute(
        "UPDATE session SET project_id='other' WHERE id='ses_original'",
        [],
    )
    .unwrap();
    fixture.assert_rejected(&record, "different project");
}

#[test]
fn historical_root_sequence_and_ambiguous_candidates() {
    let fixture = Fixture::new();
    let (record, brief) = fixture.launch();
    let db = fixture.db();
    legacy(&db);
    modern(&db, "session_v2");
    for (id, parent, fork) in [
        ("ses_original", None, None),
        ("ses_fake", None, None),
        ("ses_later", None, None),
        ("ses_fork", None, Some("ses_original")),
        ("ses_child", Some("ses_original"), None),
    ] {
        db.execute(
            "INSERT INTO session_v2 VALUES(?1,'project',?2,?3,?4)",
            params![id, fixture.cwd.to_str().unwrap(), parent, fork],
        )
        .unwrap();
    }
    modern_message(&db, "ses_original", "msg_synthetic", 0, "synthetic", &brief);
    modern_message(&db, "ses_original", "msg_first", 1, "user", &brief);
    modern_message(&db, "ses_fake", "msg_echo", 1, "assistant", &brief);
    modern_message(&db, "ses_later", "msg_earlier", 1, "user", "other");
    modern_message(&db, "ses_later", "msg_later", 2, "user", &brief);
    modern_message(&db, "ses_fork", "msg_fork", 1, "user", &brief);
    modern_message(&db, "ses_child", "msg_child", 1, "user", &brief);
    fixture.assert_resume(&record, "ses_original", "msg_first");
    db.execute(
        "INSERT INTO session VALUES('ses_duplicate','project',?1,NULL)",
        [fixture.cwd.to_str().unwrap()],
    )
    .unwrap();
    legacy_message(
        &db,
        "ses_duplicate",
        "msg_duplicate",
        1,
        "user",
        &brief,
        false,
    );
    fixture.assert_rejected(&record, "multiple root OpenCode sessions");
}

#[test]
fn unified_sessions_use_recorded_worktree_and_actual_user_sequence() {
    let fixture = Fixture::new();
    let (record, brief) = fixture.launch();
    let db = fixture.db();
    legacy(&db);
    modern(&db, "session");
    db.execute_batch(
        "UPDATE project SET worktree='/separate-main-checkout';
        CREATE TABLE project_directory(project_id TEXT,directory TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO project_directory VALUES('project',?1)",
        [fixture.cwd.to_str().unwrap()],
    )
    .unwrap();
    for id in ["ses_original", "ses_stale"] {
        db.execute(
            "INSERT INTO session VALUES(?1,'project',?2,NULL)",
            params![id, fixture.cwd.to_str().unwrap()],
        )
        .unwrap();
    }
    modern_message(&db, "ses_original", "msg_opening", 2, "user", &brief);
    modern_message(&db, "ses_stale", "msg_new_opening", 1, "user", "unrelated");
    legacy_message(&db, "ses_stale", "msg_stale", 1, "user", &brief, false);
    let saved = fixture.assert_resume(&record, "ses_original", "msg_opening");
    db.execute(
        "UPDATE session_message SET id='msg_replaced' WHERE id='msg_opening'",
        [],
    )
    .unwrap();
    let mut next = saved.clone();
    next["attempt"] = json!("a-55555555555555555555555555555555");
    let error = Options::prepare_opencode_resume(&fixture.root, &saved, &mut next)
        .err()
        .unwrap();
    assert!(error.to_string().contains("session proof changed"));
    modern_message(&db, "ses_original", "msg_same_seq", 2, "user", "other");
    fixture.assert_rejected(&record, "no root OpenCode session");
}

#[test]
fn missing_conflicting_or_unreadable_launch_evidence_stops_resume() {
    let fixture = Fixture::new();
    let (mut record, _) = fixture.launch();
    let mut legacy = record.clone();
    legacy.as_object_mut().unwrap().remove("opencodeOpening");
    fixture.assert_rejected(&legacy, "missing recorded OpenCode launch evidence");
    fixture.assert_rejected(&record, "store is missing or inaccessible");
    fs::write(&fixture.store, b"not a database").unwrap();
    fixture.assert_rejected(&record, "not a database");
    fixture.set_store(&fixture.root.join("another.db"));
    fixture.assert_rejected(&record, "store differs from the original dispatch");
    fixture.set_store(&fixture.store);
    record["opencodeOpening"]["attempt"] = json!(NEXT_ATTEMPT);
    fixture.assert_rejected(&record, "belongs to another attempt");
    record["opencodeOpening"]["attempt"] = json!(ATTEMPT);
    fs::remove_file(&fixture.store).unwrap();
    let _db = fixture.db();
    fixture.assert_rejected(&record, "unrecognized OpenCode session storage schema");
}
