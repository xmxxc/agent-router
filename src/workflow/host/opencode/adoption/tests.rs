use super::*;
use crate::workflow::host::Options;
use rusqlite::{params, Connection};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

const DISPATCH: &str = "d-33333333333333333333333333333333";
const ATTEMPT: &str = "a-11111111111111111111111111111111";
const NEXT: &str = "a-22222222222222222222222222222222";
const SECRET: &str = "synthetic-worker-capability";

struct PrivateState(PathBuf);

impl PrivateState {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("wfc-opencode-adopt-{}", Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for PrivateState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn v1_receipt_uses_committed_wal_and_survives_pending_promotion() {
    let state = PrivateState::new();
    let dir = state.0.join("dispatches");
    let cwd = state.0.join("work with space");
    fs::create_dir(&dir).unwrap();
    fs::create_dir(&cwd).unwrap();
    let config = state.0.join("launch.json");
    let inline = r#"{
        // This inline source is expanded and parsed by OpenCode.
        "permission":{"read":"ask",},
        "plugin": {env:WFC_FIXTURE_PLUGINS},
        "username":"{env:WFC_FIXTURE_USERNAME}",
    }"#;
    fs::write(
        &config,
        json!({"hosts":{"opencode":{"env":{
            "WFC_OPENCODE_ISOLATED_STORE":"1", "OPENCODE_DB":"", "WFC_OPENCODE_DB":"",
            "OPENCODE_CONFIG_CONTENT": inline,
            "OPENCODE_CONFIG":"", "OPENCODE_CONFIG_DIR":""
        }}}})
        .to_string(),
    )
    .unwrap();
    let mut row = json!({
        "dispatch": DISPATCH, "attempt": ATTEMPT, "executionId": ATTEMPT, "round": 1,
        "host": "opencode", "backend": "tmux", "role": "fixture", "scope": "fixture",
        "cwd": cwd, "task": state.0.join("task.md"), "report": state.0.join("report.md")
    });
    let options = Options {
        name: "opencode",
        backend: "tmux",
        config: config.to_str(),
        ..Options::default()
    };
    let launch = options.prepare(&state.0, &mut row).unwrap();
    let mut packet = Receipt {
        dispatch: DISPATCH.into(),
        attempt: ATTEMPT.into(),
        cwd,
        database: PathBuf::from(launch.env["OPENCODE_DB"].as_str().unwrap()),
        session: "ses_original".into(),
        first_message: "msg_original".into(),
    };
    let capability_file = packet.database.with_file_name(CAPABILITY);
    let capability = fs::read_to_string(&capability_file).unwrap();
    fs::remove_file(&capability_file).unwrap();
    let notify = |packet: &Receipt, secret: &str| publish(&state.0, packet, secret, &capability);
    let brief = launch.argv.last().unwrap();
    let plugin = packet.database.with_file_name(PLUGIN);
    assert_eq!(fs::metadata(&plugin).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::read(&plugin).unwrap(), PLUGIN_SOURCE);
    assert!(launch.env["OPENCODE_CONFIG_CONTENT"] == inline);
    let overlay: Value =
        serde_json::from_slice(&fs::read(launch.env["OPENCODE_CONFIG"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(overlay["plugin"][0][0], json!(plugin));
    assert!(overlay["permission"].is_null());
    assert!(!overlay.to_string().contains(SECRET));
    assert!(!overlay.to_string().contains(&capability));
    row[controller::CALLBACK_HASH] = json!(messages::body_hash(SECRET));
    let pending = {
        let _lock = messages::lock_for_private_publish(&state.0).unwrap();
        assignment::reserve(&dir, DISPATCH, &row).unwrap()
    };
    let original = fs::read(&pending).unwrap();
    let mut db = Connection::open(&packet.database).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE project(id TEXT PRIMARY KEY,worktree TEXT NOT NULL);
         CREATE TABLE session(id TEXT PRIMARY KEY,project_id TEXT,directory TEXT,parent_id TEXT);
         CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
         CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,data TEXT);
         CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT REFERENCES session(id),
             type TEXT,seq INTEGER,data TEXT);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO project VALUES('project',?1)",
        [packet.cwd.to_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO session VALUES('ses_original','project',?1,NULL)",
        [packet.cwd.to_str().unwrap()],
    )
    .unwrap();
    let store = packet.database.clone();
    let store_files = || ["db", "db-wal", "db-shm"].map(|suffix| store.with_extension(suffix));
    assert!(store_files().iter().all(|file| file.is_file()));
    assert_eq!(fs::read(&pending).unwrap(), original); // DB alone issues no receipt.

    let transaction = db.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO message VALUES('msg_original','ses_original',1,?1)",
            [r#"{"role":"user"}"#],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO part VALUES('prt_original','msg_original','ses_original',?1)",
            [json!({"type":"text","text":brief}).to_string()],
        )
        .unwrap();
    assert!(notify(&packet, SECRET).is_err());
    assert_eq!(fs::read(&pending).unwrap(), original);
    transaction.commit().unwrap();

    assert!(notify(&packet, "another-worker").is_err());
    assert!(publish(&state.0, &packet, SECRET, "").is_err());
    assert!(publish(&state.0, &packet, SECRET, &"f".repeat(64)).is_err());
    let inbox = state.0.join("messages.lock");
    let parked_lock = state.0.join("parked-lock");
    fs::rename(&inbox, &parked_lock).unwrap();
    std::os::unix::fs::symlink(&parked_lock, &inbox).unwrap();
    assert!(notify(&packet, SECRET).is_err());
    fs::remove_file(&inbox).unwrap();
    fs::rename(&parked_lock, &inbox).unwrap();
    packet.attempt = NEXT.into();
    assert!(notify(&packet, SECRET).is_err());
    packet.attempt = ATTEMPT.into();
    packet.cwd = state.0.clone();
    assert!(notify(&packet, SECRET).is_err());
    packet.cwd = PathBuf::from(row["cwd"].as_str().unwrap());
    let other = packet.database.with_file_name("other.db");
    fs::copy(&packet.database, &other).unwrap();
    let real = std::mem::replace(&mut packet.database, other);
    assert!(notify(&packet, SECRET).is_err());
    packet.database = real;
    fs::set_permissions(&state.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(notify(&packet, SECRET).is_err());
    fs::set_permissions(&state.0, fs::Permissions::from_mode(0o700)).unwrap();

    let same_id_other_source = || {
        db.execute(
            "INSERT INTO session_message VALUES(?1,?2,'user',1,?3)",
            params![
                packet.first_message,
                packet.session,
                json!({"text":brief}).to_string()
            ],
        )
        .unwrap();
    };
    same_id_other_source();
    assert!(notify(&packet, SECRET).is_err());
    assert_eq!(fs::read(&pending).unwrap(), original);
    db.execute("DELETE FROM session_message", []).unwrap();
    let [main, wal, _shm] = store_files();
    let db_before = [fs::read(&main).unwrap(), fs::read(&wal).unwrap()];
    notify(&packet, SECRET).unwrap();
    let adopted = fs::read(&pending).unwrap();
    assert_ne!(adopted, original);
    assert_eq!(
        [fs::read(&main).unwrap(), fs::read(&wal).unwrap()],
        db_before
    );
    assert!(store_files().iter().all(|file| file.is_file()));

    let published = pending.with_extension("json");
    {
        let _lock = messages::lock_for_private_publish(&state.0).unwrap();
        preserve_pending(&pending, &mut row).unwrap();
        row["create"] = json!({"result":"accepted"});
        write_json(&published, &row, None).unwrap();
        fs::remove_file(&pending).unwrap();
    }
    assert_eq!(row["hostSession"], packet.session);
    assert_eq!(row["opencodeSession"]["storageSource"], "legacy");
    assert_eq!(row["opencodeSession"]["firstMessage"], packet.first_message);
    let published_bytes = fs::read(&published).unwrap();
    notify(&packet, SECRET).unwrap();
    same_id_other_source();
    assert!(notify(&packet, SECRET).is_err());
    db.execute("DELETE FROM session_message", []).unwrap();
    packet.first_message = "msg_copy".into();
    assert!(notify(&packet, SECRET).is_err());
    assert_eq!(fs::read(&published).unwrap(), published_bytes);
    assert!(!pending.exists());
}
