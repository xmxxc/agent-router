use super::super::private_file;
use crate::workflow::write_json;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, TryLockError},
    io::{self, Read},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const INITIAL_SECONDS: u64 = 2;
const MAX_SECONDS: u64 = 16;

pub(super) struct Backoff(PathBuf);

#[derive(Deserialize, Serialize)]
struct Cooldown {
    boot_id: String,
    started_ms: u64,
    seconds: u64,
}

impl Backoff {
    pub(super) fn new(state: &Path, thread: &str) -> Result<Self> {
        Ok(Self(state.join(format!(
            "relay-codex-{}.json",
            Uuid::parse_str(thread)?
        ))))
    }

    pub(super) fn try_ready(&self) -> Result<Option<File>> {
        // The stable lock is independent of the atomically replaced cooldown.
        let lock = super::super::open_relay_file(private_file().create(true))
            .open(self.0.with_extension("lock"))?;
        ensure!(
            lock.metadata()?.is_file(),
            "invalid Codex relay target lock"
        );
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if let Some(cooldown) = self.read()? {
            let (boot_id, now_ms) = boot_clock()?;
            if cooldown.boot_id == boot_id
                && now_ms
                    .checked_sub(cooldown.started_ms)
                    .is_some_and(|elapsed| elapsed < cooldown.seconds * 1000)
            {
                return Ok(None);
            }
        }
        Ok(Some(lock))
    }

    fn read(&self) -> Result<Option<Cooldown>> {
        let file =
            match super::super::open_relay_file(fs::OpenOptions::new().read(true)).open(&self.0) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                other => other?,
            };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 512,
            "invalid Codex relay cooldown"
        );
        let value: Cooldown = serde_json::from_reader(file.take(512))?;
        ensure!(
            (INITIAL_SECONDS..=MAX_SECONDS).contains(&value.seconds),
            "invalid Codex relay delay"
        );
        Ok(Some(value))
    }

    pub(super) fn record_full(&self) -> Result<u64> {
        let (boot_id, started_ms) = boot_clock()?;
        let seconds = self
            .read()?
            .filter(|prior| prior.boot_id == boot_id)
            .map_or(INITIAL_SECONDS, |prior| {
                (prior.seconds * 2).min(MAX_SECONDS)
            });
        let cooldown = Cooldown {
            boot_id,
            started_ms,
            seconds,
        };
        write_json(&self.0, &serde_json::to_value(cooldown)?, None)?;
        Ok(seconds)
    }

    pub(super) fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.0) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

// Unlike wall time, this clock stays monotonic across relay restarts.
#[cfg(target_os = "linux")]
fn boot_clock() -> io::Result<(String, u64)> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid writable timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        boot_id,
        now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000,
    ))
}

#[cfg(windows)]
fn boot_clock() -> io::Result<(String, u64)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(("windows-wall-clock".into(), now.as_millis() as u64))
}
