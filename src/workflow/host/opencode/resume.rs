//! The private handoff from the public lifecycle to the resumed OpenCode TUI.
use super::{
    adoption, launch_directory, open_read_only, read_session, saved_proof, store_directory,
    verify_opening, StorageSource,
};
use crate::workflow::{host::Launch, messages};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PLUGIN: &str = "wfc-opencode-resume.mjs";
const MAX_PLAN: u64 = 64 * 1024;
const INPUT_WINDOW_MS: i64 = 15_000;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Session {
    dispatch: String,
    attempt: String,
    round: String,
    cwd: PathBuf,
    database: PathBuf,
    session: String,
}

impl Session {
    fn from_row(state: &Path, row: &Value) -> Result<Self> {
        let opening = verify_opening(state, row)?;
        let proof = saved_proof(row, &opening)?;
        ensure!(
            row["host"] == "opencode"
                && row["backend"] == "tmux"
                && row["executionId"] == proof.current_attempt
                && proof.storage_source == StorageSource::Legacy,
            "missing verified OpenCode v1 adoption"
        );
        Ok(Self {
            dispatch: opening.dispatch,
            attempt: proof.current_attempt,
            round: row["round"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("invalid round")?
                .to_string(),
            cwd: opening.cwd,
            database: opening.database,
            session: proof.session,
        })
    }

    fn directory(&self, state: &Path) -> Result<PathBuf> {
        launch_directory(state, &self.dispatch, &self.attempt)
    }
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Plan {
    launch: Session,
    brief: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Permit {
    plan_sha256: String,
    expires_at: i64,
}

pub(in crate::workflow) struct Prepared {
    dir: PathBuf,
    identity: (u64, u64),
    plan: Plan,
    published: bool,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.published
            && store_directory(&self.dir)
                .is_ok_and(|info| (info.dev(), info.ino()) == self.identity)
        {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

impl Prepared {
    pub(in crate::workflow) fn new(
        state: &Path,
        next: &Value,
        launch: &mut Launch,
        brief: String,
    ) -> Result<Self> {
        adoption::require_private_state(state)?;
        let session = Session::from_row(state, next)?;
        let plan = Plan {
            launch: session,
            brief,
        };
        let wire = serde_json::to_vec(&plan)?;
        ensure!(
            wire.len() as u64 <= MAX_PLAN,
            "OpenCode resume brief too large"
        );
        let dir = plan.launch.directory(state)?;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let info = store_directory(&dir)?;
        let prepared = Self {
            dir,
            identity: (info.dev(), info.ino()),
            plan,
            published: false,
        };
        let dir = &prepared.dir;
        adoption::write_private(&dir.join("resume.json"), &wire)?;
        adoption::write_private(&dir.join(PLUGIN), include_bytes!("resume.mjs"))?;
        adoption::add_plugin_config(
            dir,
            &mut launch.env,
            "OPENCODE_TUI_CONFIG",
            "tui.json",
            json!(dir.join(PLUGIN)),
        )?;
        adoption::create_capability(dir)?;
        messages::sync_directory(dir)?;
        messages::sync_directory(state)?;
        Ok(prepared)
    }

    // Called inside the shared assignment publication transaction.
    pub(in crate::workflow) fn recheck(
        &self,
        state: &Path,
        previous: &Value,
        next: &Value,
    ) -> Result<()> {
        let old = Session::from_row(state, previous)?;
        let new = Session::from_row(state, next)?;
        ensure!(
            self.plan.launch == new
                && old.session == new.session
                && old.database == new.database
                && old.attempt != new.attempt
                && read_private::<Plan>(&self.dir.join("resume.json"))? == self.plan,
            "OpenCode resume plan changed"
        );
        ensure!(
            !query_delivery(state, previous, &self.plan.brief)?,
            "brief already exists"
        );
        Ok(())
    }

    pub(in crate::workflow) fn published(&mut self) {
        self.published = true;
    }

    // The controller calls this with the Inbox lock after backend create succeeds.
    pub(in crate::workflow) fn authorize_input(&self) -> Result<i64> {
        let permit = Permit {
            plan_sha256: plan_hash(&self.plan)?,
            expires_at: chrono::Utc::now().timestamp_millis() + INPUT_WINDOW_MS,
        };
        adoption::write_private(&self.dir.join("permitted"), &serde_json::to_vec(&permit)?)?;
        messages::sync_directory(&self.dir)?;
        Ok(permit.expires_at)
    }

    pub(in crate::workflow) fn adopted(&self) -> bool {
        read_private::<Session>(&self.dir.join("claimed"))
            .is_ok_and(|receipt| receipt == self.plan.launch)
    }

    pub(in crate::workflow) fn delivered(&self, state: &Path, next: &Value) -> Result<bool> {
        ensure!(
            self.plan.launch == Session::from_row(state, next)?,
            "OpenCode resume changed"
        );
        query_delivery(state, next, &self.plan.brief)
    }

    pub(in crate::workflow) fn wait_for_delivery(
        &self,
        state: &Path,
        next: &Value,
        expires_at: i64,
    ) {
        let start = Instant::now();
        while chrono::Utc::now().timestamp_millis() < expires_at
            && start.elapsed() < Duration::from_millis(INPUT_WINDOW_MS as u64)
        {
            if self.adopted() && self.delivered(state, next).is_ok_and(|done| done) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn plan_hash(plan: &Plan) -> Result<String> {
    Ok(messages::body_hash(&serde_json::to_string(plan)?))
}

fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.nlink() == 1
            && info.mode() & 0o777 == 0o600
            && info.uid() == unsafe { libc::geteuid() }
            && info.len() <= MAX_PLAN,
        "invalid private OpenCode resume file"
    );
    Ok(serde_json::from_reader(file.take(MAX_PLAN + 1))?)
}

fn query_delivery(state: &Path, row: &Value, brief: &str) -> Result<bool> {
    let opening = verify_opening(state, row)?;
    let proof = saved_proof(row, &opening)?;
    ensure!(
        proof.storage_source == StorageSource::Legacy,
        "unsupported OpenCode resume source"
    );
    let mut db = open_read_only(&opening)?;
    let snapshot = db.transaction()?;
    ensure!(
        read_session(
            &snapshot,
            &opening,
            &proof.current_attempt,
            &proof.session,
            &proof.first_message,
            Some(proof.storage_source)
        )? == proof,
        "original OpenCode conversation changed"
    );
    let count: u32 = snapshot.query_row(
        "SELECT COUNT(*) FROM message m WHERE m.session_id=?1 AND m.id<>?2
         AND json_extract(m.data,'$.role')='user'
         AND ?3=(SELECT CASE WHEN COUNT(*)=1 THEN MAX(wfc_opening_digest(json_extract(p.data,'$.text'))) END
                 FROM part p WHERE p.session_id=m.session_id AND p.message_id=m.id
                 AND json_extract(p.data,'$.type')='text'
                 AND json_type(p.data,'$.text')='text'
                 AND COALESCE(json_extract(p.data,'$.synthetic'),0)=0
                 AND COALESCE(json_extract(p.data,'$.ignored'),0)=0)",
        rusqlite::params![proof.session, proof.first_message, messages::body_hash(brief)],
        |r| r.get(0),
    )?;
    ensure!(count < 2, "more than one committed OpenCode resume brief");
    Ok(count == 1)
}
