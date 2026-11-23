//! Offline access to workflow-core's Python router.db. See the crate README for cutover.
mod completion;
mod migrate;
mod preview;
mod recovery;
mod schema;

pub use migrate::open_offline_copy;
pub use preview::{reclaim_preview, status, verify_router_db};

use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

/// Resolve links in the existing prefix without creating a missing state directory.
fn resolve(path: &Path) -> Result<PathBuf> {
    resolve_absolute(&std::path::absolute(path)?, 0)
}

fn resolve_absolute(absolute: &Path, links: usize) -> Result<PathBuf> {
    ensure!(
        links <= 40,
        "too many symbolic links resolving {}",
        absolute.display()
    );
    let mut result = PathBuf::new();
    for part in absolute.components() {
        if part == std::path::Component::ParentDir {
            result.pop();
        } else {
            result.push(part);
            match fs::canonicalize(&result) {
                Ok(path) => result = path,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // canonicalize cannot follow a link whose target is still missing.
                    if let Ok(target) = fs::read_link(&result) {
                        result.pop();
                        result = resolve_absolute(&result.join(target), links + 1)?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(result)
}

fn expand_home(path: PathBuf) -> Result<PathBuf> {
    let mut parts = path.components();
    let head = parts.find(|part| *part != std::path::Component::CurDir);
    let Some(std::path::Component::Normal(first)) = head else {
        return Ok(path);
    };
    if !first.as_encoded_bytes().starts_with(b"~") {
        return Ok(path);
    }
    let name = first
        .to_str()
        .context("cannot determine home directory for non-UTF-8 account name")?;
    let home = if name == "~" {
        env::home_dir().context("cannot determine home directory")?
    } else {
        #[cfg(unix)]
        {
            let why = || format!("cannot determine home directory for {name}");
            nix::unistd::User::from_name(&name[1..])
                .with_context(why)?
                .with_context(why)?
                .dir
        }
        #[cfg(not(unix))]
        anyhow::bail!("cannot determine home directory for {name}")
    };
    Ok(home.join(parts.as_path()))
}

/// Python WFC_STATE discovery, independent of the Rust foreground JSON store.
pub fn state_dir() -> Result<PathBuf> {
    let home = || env::home_dir().context("cannot determine home directory");
    let path = match env::var_os("WFC_STATE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        Some(path) => expand_home(path)?,
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
