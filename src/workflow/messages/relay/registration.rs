//! The live relay holds an exclusive flock for its lifetime. A crashed process
//! releases it in the kernel, including when a PID is subsequently reused.
#[cfg(target_os = "linux")]
use super::super::private_file;
use super::super::{receipt, ErrorKind};
use anyhow::Result;
use serde_json::{json, Value};
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::{File, TryLockError};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) struct Registration {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    _lock: File,
}

#[cfg(windows)]
fn staged_path(dir: &Path, identity: &Value) -> PathBuf {
    let creation = identity["creation"].as_str().expect("process creation time");
    dir.join(format!("detached-{}-{creation}.json", identity["pid"]))
}

impl Registration {

    pub(super) fn create(
        state: &Path,
        scope: Option<&str>,
        (field, id): (&str, &str),
        #[cfg(windows)] detached: bool,
    ) -> Result<Self> {
        let dir = state.join("relays");
        fs::create_dir_all(&dir)?;
        #[cfg(windows)]
        {
            let process = crate::windows_process::current_identity()?;
            let path = if detached {
                staged_path(&dir, &process)
            } else {
                dir.join(format!("{}.json", Uuid::new_v4()))
            };
            crate::workflow::write_json(
                &path,
                &json!({"kind":"relay","pid":std::process::id(),
                "since":receipt::timestamp(),"scope":scope,(field):id,
                "process":process}),
                None,
            )?;
            Ok(Self { path })
        }
        #[cfg(target_os = "linux")]
        {
            let path = dir.join(format!("{}.json", Uuid::new_v4()));
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
}

#[cfg(windows)]
pub(super) fn staged(
    state: &Path,
    identity: &Value,
    scope: Option<&str>,
    (field, id): (&str, &str),
) -> Result<bool> {
    let path = staged_path(&state.join("relays"), identity);
    let file = match super::super::open_relay_file(fs::OpenOptions::new().read(true)).open(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    use std::os::windows::fs::MetadataExt;
    use winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "invalid detached relay registration file"
    );
    let entry: Value = serde_json::from_reader(file).map_err(|_| {
        anyhow::anyhow!("invalid detached relay registration record")
    })?;
    anyhow::ensure!(
        entry["kind"] == "relay" && entry["pid"] == identity["pid"]
            && entry["process"] == *identity && entry["scope"] == json!(scope) && entry[field] == id,
        "detached relay registration identity mismatch"
    );
    Ok(true)
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
        let file = match super::super::open_relay_file(fs::OpenOptions::new().read(true)).open(path)
        {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            other => other?,
        };
        if !file.metadata()?.is_file() {
            continue;
        }
        #[cfg(target_os = "linux")]
        match file.try_lock_shared() {
            Ok(()) => continue,
            Err(TryLockError::WouldBlock) => active.push(serde_json::from_reader(file)?),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        #[cfg(windows)]
        {
            use crate::windows_process as process;
            use std::os::windows::io::AsRawHandle;
            let entry: Value = serde_json::from_reader(file)?;
            if let Ok(Some(owned)) = process::open(&entry["process"]) {
                if process::exit_code(owned.as_raw_handle())?.is_none() {
                    active.push(entry);
                }
            }
        }
    }
    active.sort_by(|a, b| a["since"].as_str().cmp(&b["since"].as_str()));
    Ok(active)
}
