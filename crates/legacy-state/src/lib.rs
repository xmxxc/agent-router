//! Offline access to workflow-core's Python router.db. See the crate README for cutover.
mod migrate;
mod preview;
mod schema;

pub use migrate::open_offline_copy;
pub use preview::{reclaim_preview, status};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

/// Resolve links in the existing prefix without creating a missing state directory.
fn resolve(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for part in absolute.components() {
        if part == std::path::Component::ParentDir {
            result.pop();
        } else {
            result.push(part);
            match fs::canonicalize(&result) {
                Ok(path) => result = path,
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(result)
}

/// Python WFC_STATE discovery, independent of the Rust foreground JSON store.
pub fn state_dir() -> Result<PathBuf> {
    let home = || env::home_dir().context("cannot determine home directory");
    let path = match env::var_os("WFC_STATE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        Some(path) if path.starts_with("~") => home()?.join(path.strip_prefix("~")?),
        Some(path) => path,
        None if cfg!(windows) => env::var_os("LOCALAPPDATA")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or(home()?.join("AppData/Local"))
            .join("workflow-core"),
        None => home()?.join(".local/state/workflow-core"),
    };
    resolve(&path)
}

fn open(path: &Path, flags: OpenFlags) -> Result<Connection> {
    let db = Connection::open_with_flags(path, flags)
        .with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.execute_batch("PRAGMA trusted_schema=OFF")?;
    Ok(db)
}
