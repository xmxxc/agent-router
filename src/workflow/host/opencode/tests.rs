use super::*;
use crate::workflow::host::{Launch, Options};
use uuid::Uuid;

const ATTEMPT: &str = "a-11111111111111111111111111111111";
const DISPATCH: &str = "d-33333333333333333333333333333333";
const NEXT_ATTEMPT: &str = "a-22222222222222222222222222222222";
const RETRY_ATTEMPT: &str = "a-33333333333333333333333333333333";

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
        let store = launch_directory(&root, DISPATCH, ATTEMPT)
            .unwrap()
            .join("opencode.db");
        let fixture = Self {
            root,
            cwd,
            config,
            store,
        };
        fixture.configure(
            json!({"WFC_OPENCODE_ISOLATED_STORE":"1","OPENCODE_DB":"","WFC_OPENCODE_DB":""}),
        );
        fixture
    }

    fn configure(&self, env: Value) {
        fs::write(
            &self.config,
            json!({"hosts":{"opencode":{"command":["opencode-fixture"],"env":env}}}).to_string(),
        )
        .unwrap();
    }

    fn set_store(&self, store: &Path) {
        self.configure(json!({"OPENCODE_DB":store,"WFC_OPENCODE_DB":store,
                              "WFC_OPENCODE_ISOLATED_STORE":"0"}));
    }

    fn start(&self) -> Result<(Value, Launch)> {
        let mut record = json!({
            "dispatch":DISPATCH, "attempt": ATTEMPT, "round":1,
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
        .prepare(&self.root, &mut record)?;
        Ok((record, launch))
    }

    fn launch(&self) -> (Value, String) {
        let (record, launch) = self.start().unwrap();
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
        assert!(!self.store.exists());
        assert_eq!(
            fs::read_dir(self.store.parent().unwrap()).unwrap().count(),
            0
        );
        (record, brief)
    }

    fn resume(&self, original: &Value) -> Result<(Launch, String, Value)> {
        let mut next = original.clone();
        next["attempt"] = json!(if original["attempt"] == NEXT_ATTEMPT {
            RETRY_ATTEMPT
        } else {
            NEXT_ATTEMPT
        });
        next["round"] = json!(2);
        let (launch, input) = Options::prepare_opencode_resume(&self.root, original, &mut next)?;
        Ok((launch, input, next))
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

    fn receipt<'a>(&'a self, session: &'a str, first_message: &'a str) -> OpencodeAdoption<'a> {
        OpencodeAdoption {
            dispatch: DISPATCH,
            attempt: ATTEMPT,
            cwd: &self.cwd,
            database: &self.store,
            session,
            first_message,
        }
    }

    fn adopted(&self, original: &Value, id: &str, message: &str) -> Value {
        let mut record = original.clone();
        adopt_opencode_session(&self.root, &mut record, self.receipt(id, message)).unwrap();
        record
    }

    fn assert_resume(&self, original: &Value, id: &str, message: &str) -> Value {
        let before = fs::read(&self.store).unwrap();
        self.assert_rejected(
            original,
            "missing verified original OpenCode adoption receipt",
        );
        let adopted = self.adopted(original, id, message);
        let (launch, input, next) = self.resume(&adopted).unwrap();
        assert!(input.ends_with(&format!("Current attempt {NEXT_ATTEMPT}.")));
        assert!(!input.contains(ATTEMPT));
        assert!(!launch.argv.contains(&input));
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
            json!({"project":"project","firstMessage":message,"adoptedAttempt":ATTEMPT,"currentAttempt":NEXT_ATTEMPT,"session":id})
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
    let (launch, _) =
        Options::prepare_opencode_resume(&fixture.root, &saved, &mut recheck).unwrap();
    assert_eq!(launch.argv.last().unwrap(), "ses_original");
    db.execute(
        "UPDATE session SET project_id='other' WHERE id='ses_original'",
        [],
    )
    .unwrap();
    fixture.assert_rejected(&saved, "different project");
}

#[test]
fn historical_root_sequence_and_adopted_id_ignore_other_roots() {
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
    fixture.assert_resume(&record, "ses_original", "msg_first");
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
    assert!(error
        .to_string()
        .contains("invalid OpenCode session identity"));
    modern_message(&db, "ses_original", "msg_same_seq", 2, "user", "other");
    fixture.assert_rejected(&saved, "does not match the original first user message");
}

#[test]
fn missing_conflicting_or_unreadable_launch_evidence_stops_resume() {
    let fixture = Fixture::new();
    let (mut record, brief) = fixture.launch();
    for (content, diagnostic) in [
        (None, "store is missing or inaccessible"),
        (Some(b"not a database".as_slice()), "not a database"),
    ] {
        if let Some(content) = content {
            fs::write(&fixture.store, content).unwrap();
        }
        let error = adopt_opencode_session(
            &fixture.root,
            &mut record,
            fixture.receipt("ses_original", "msg_first"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(diagnostic), "{error:#}");
    }
    fs::remove_file(&fixture.store).unwrap();
    let db = fixture.db();
    let error = adopt_opencode_session(
        &fixture.root,
        &mut record,
        fixture.receipt("ses_original", "msg_first"),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("unrecognized OpenCode session storage schema"));
    legacy(&db);
    put_legacy_root(&fixture, &db, "ses_original", "msg_first", &brief);
    let adopted = fixture.adopted(&record, "ses_original", "msg_first");
    fixture.set_store(&fixture.root.join("another.db"));
    fixture.assert_rejected(&adopted, "store differs from the original dispatch");
    fixture.set_store(&fixture.store);
    record["opencodeOpening"]["attempt"] = json!(NEXT_ATTEMPT);
    fixture.assert_rejected(&record, "does not match the recorded launch");
}

fn put_legacy_root(fixture: &Fixture, db: &Connection, id: &str, message: &str, brief: &str) {
    db.execute(
        "INSERT INTO session VALUES(?1,'project',?2,NULL)",
        params![id, fixture.cwd.to_str().unwrap()],
    )
    .unwrap();
    legacy_message(db, id, message, 1, "user", brief, false);
}

#[test]
fn lone_unmarked_legacy_fork_cannot_be_adopted_by_lookup() {
    let fixture = Fixture::new();
    let (record, brief) = fixture.launch();
    let db = fixture.db();
    legacy(&db);
    put_legacy_root(&fixture, &db, "ses_original", "msg_first", &brief);
    let adopted = fixture.adopted(&record, "ses_original", "msg_first");
    put_legacy_root(&fixture, &db, "ses_copy", "msg_copy", &brief);
    db.execute("DELETE FROM session WHERE id='ses_original'", [])
        .unwrap();
    fixture.assert_rejected(
        &record,
        "missing verified original OpenCode adoption receipt",
    );
    fixture.assert_rejected(&adopted, "does not match the original first user message");
}

#[test]
fn private_launch_and_adoption_identity_are_bound() {
    let fixture = Fixture::new();
    let shared = fixture.root.join("external.db");
    fs::write(&shared, b"external fixture must never be opened").unwrap();
    fixture.set_store(&shared);
    let (old, launch) = fixture.start().unwrap();
    assert_eq!(launch.env["OPENCODE_DB"], json!(shared));
    assert_eq!(
        fs::read(&shared).unwrap(),
        b"external fixture must never be opened"
    );
    fixture.assert_rejected(&old, "missing private OpenCode opening evidence");
    let mut legacy_record = old.clone();
    legacy_record["opencodeOpening"] =
        json!({"attempt":ATTEMPT,"database":shared,"briefSha256":"old"});
    legacy_record["hostSession"] = json!("ses_original");
    legacy_record["opencodeSession"] = json!({"firstMessage":"msg_first","project":"project"});
    fixture.assert_rejected(&legacy_record, "missing private OpenCode opening evidence");

    fixture.configure(
        json!({"WFC_OPENCODE_ISOLATED_STORE":"1","OPENCODE_DB":shared,"WFC_OPENCODE_DB":""}),
    );
    assert!(fixture
        .start()
        .err()
        .unwrap()
        .to_string()
        .contains("cannot use a configured database"));
    fixture.configure(
        json!({"WFC_OPENCODE_ISOLATED_STORE":"1","OPENCODE_DB":"","WFC_OPENCODE_DB":""}),
    );
    let (mut record, brief) = fixture.launch();
    assert!(fixture
        .start()
        .err()
        .unwrap()
        .to_string()
        .contains("cannot reserve a fresh private"));
    let db = fixture.db();
    legacy(&db);
    put_legacy_root(&fixture, &db, "ses_original", "msg_first", &brief);
    put_legacy_root(&fixture, &db, "ses_copy", "msg_copy", &brief);

    for receipt in [
        OpencodeAdoption {
            attempt: NEXT_ATTEMPT,
            ..fixture.receipt("ses_original", "msg_first")
        },
        OpencodeAdoption {
            cwd: &fixture.root,
            ..fixture.receipt("ses_original", "msg_first")
        },
        OpencodeAdoption {
            database: &shared,
            ..fixture.receipt("ses_original", "msg_first")
        },
        OpencodeAdoption {
            dispatch: "d-12345678",
            ..fixture.receipt("ses_original", "msg_first")
        },
        fixture.receipt("ses_original", "msg_copy"),
    ] {
        assert!(adopt_opencode_session(&fixture.root, &mut record, receipt).is_err());
        assert!(record["opencodeSession"].is_null());
    }
    record = fixture.adopted(&record, "ses_original", "msg_first");
    adopt_opencode_session(
        &fixture.root,
        &mut record,
        fixture.receipt("ses_original", "msg_first"),
    )
    .unwrap();
    let error = adopt_opencode_session(
        &fixture.root,
        &mut record,
        fixture.receipt("ses_copy", "msg_copy"),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("conflicts with the recorded session"));
    let mut stale = record.clone();
    stale["attempt"] = json!(RETRY_ATTEMPT);
    fixture.assert_rejected(&stale, "adoption conflicts");
    let mut mismatched = record.clone();
    mismatched["hostSession"] = json!("ses_copy");
    fixture.assert_rejected(&mismatched, "adoption conflicts");
    fs::rename(fixture.store.parent().unwrap(), fixture.root.join("parked")).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.store.parent().unwrap())
        .unwrap();
    fixture.assert_rejected(&record, "private OpenCode launch directory changed");
}
