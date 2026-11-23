//! An unnamed wait can resume a bounded traversal after the CLI exits.
use super::{super::body_hash, Candidate, Scan, RECORDS_PER_SLICE, SLICE};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    ffi::CStr,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, IntoRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    ptr::NonNull,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(PartialEq, Eq, Deserialize, Serialize)]
struct Position {
    device: u64,
    inode: u64,
    offset: u64,
    revision: String,
}

fn load(path: &Path) -> Option<Position> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(512).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(in crate::workflow::messages) struct Advance {
    path: PathBuf,
    previous: Option<Position>,
    next: Position,
}

impl Advance {
    // The inbox lock serializes this compare-and-set with other waits. Save
    // any notices first; a lost checkpoint only causes a bounded retry.
    pub(in crate::workflow::messages) fn commit(self, state: &Path) {
        if load(&self.path) == self.previous
            && crate::workflow::write_json(&self.path, &json!(self.next), None).is_ok()
        {
            let _ = super::super::sync_directory(state);
        }
    }
}

struct Directory(NonNull<libc::DIR>);

impl Directory {
    fn open(path: &Path, previous: Option<&Position>) -> Result<(Self, u64, u64)> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        let (device, inode) = (metadata.dev(), metadata.ino());
        if let Some(previous) =
            previous.filter(|value| value.device == device && value.inode == inode)
        {
            // Linux directory offsets are opaque cookies. If this filesystem
            // rejects a saved cookie, start a new sweep on the same directory.
            if file.seek(SeekFrom::Start(previous.offset)).is_err() {
                file.seek(SeekFrom::Start(0))?;
            }
        }
        // On success fdopendir takes ownership, including on closedir.
        let dir = NonNull::new(unsafe { libc::fdopendir(file.as_raw_fd()) })
            .ok_or_else(io::Error::last_os_error)
            .context("open startup recovery directory stream")?;
        let _ = file.into_raw_fd();
        Ok((Self(dir), device, inode))
    }

    fn next(&mut self) -> Option<(&[u8], u64)> {
        // readdir's buffer is borrowed only until the next operation on this
        // stream. d_off is the cookie for resuming after the returned entry.
        let entry = unsafe { libc::readdir64(self.0.as_ptr()).as_ref()? };
        let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
        Some((name.to_bytes(), entry.d_off as u64))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

pub(in crate::workflow::messages) struct Batch {
    pub(in crate::workflow::messages) candidates: Vec<Candidate>,
    pub(in crate::workflow::messages) advance: Option<Advance>,
    pub(in crate::workflow::messages) more: bool,
}

impl Scan {
    pub(in crate::workflow::messages) fn poll(
        &mut self,
        state: &Path,
        dispatch: Option<&str>,
        scope: Option<&str>,
        allowance: Duration,
    ) -> Batch {
        let mut batch = Batch {
            candidates: Vec::new(),
            advance: None,
            more: false,
        };
        if allowance.is_zero() {
            return batch;
        }
        if dispatch.is_some() {
            batch.candidates = self.batch(state, dispatch, scope, allowance);
            return batch;
        }

        let start = Instant::now();
        let dir = state.join("dispatches");
        let path = state.join(scope.map_or_else(
            || "startup-scan.json".to_owned(),
            |scope| format!("startup-scan-{}.json", body_hash(scope)),
        ));
        let previous = load(&path);
        let Ok((mut directory, device, inode)) = Directory::open(&dir, previous.as_ref()) else {
            return batch;
        };
        let mut offset = 0;
        for index in 0..RECORDS_PER_SLICE {
            // Even a zero-time poll attempts one entry; after that honor both
            // the record and elapsed budgets.
            if index > 0 && start.elapsed() >= allowance.min(SLICE) {
                break;
            }
            let Some((name, next)) = directory.next() else {
                offset = 0;
                break;
            };
            offset = next;
            if let Some(id) = super::dispatch_filename(name) {
                batch
                    .candidates
                    .extend(self.candidate(state, &dir, id, scope));
            }
        }
        batch.more = offset != 0;
        if offset != 0 || previous.as_ref().is_some_and(|saved| saved.offset != 0) {
            batch.advance = Some(Advance {
                path,
                previous,
                next: Position {
                    device,
                    inode,
                    offset,
                    revision: Uuid::new_v4().simple().to_string(),
                },
            });
        }
        batch
    }
}
