//! The live relay holds an exclusive flock for its lifetime. A crashed process
//! releases it in the kernel, including when a PID is subsequently reused.
use super::super::{private_file, receipt, ErrorKind};
use anyhow::Result;
use serde_json::{json, Value};
use std::fs::{self, File, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) struct Registration {
    path: PathBuf,
    _lock: File,
}

impl Registration {
    pub fn create(state: &Path, scope: Option<&str>, (field, id): (&str, &str)) -> Result<Self> {
        let dir = state.join("relays");
        fs::create_dir_all(&dir)?;
        let path = dir.join(Uuid::new_v4().to_string()).with_extension("json");
        let temp = path.with_extension("tmp");
        let file = private_file().create_new(true).open(&temp)?;
        let mut entry = Self {
            path: temp,
            _lock: file,
        };
        entry._lock.lock()?;
        serde_json::to_writer(
            &entry._lock,
            &json!({"kind":"relay","pid":std::process::id(),
            "since":receipt::timestamp(),"scope":scope,(field):id}),
        )?;
        fs::rename(&entry.path, &path)?;
        entry.path = path;
        Ok(entry)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn waiters(state: &Path) -> Result<Vec<Value>> {
    let mut active: Vec<Value> = Vec::new();
    let entries = match fs::read_dir(state.join("relays")) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(active),
        other => other?,
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            other => other?,
        };
        if !file.metadata()?.is_file() {
            continue;
        }
        match file.try_lock_shared() {
            Ok(()) => continue,
            Err(TryLockError::WouldBlock) => active.push(serde_json::from_reader(file)?),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    active.sort_by(|a, b| a["since"].as_str().cmp(&b["since"].as_str()));
    Ok(active)
}
