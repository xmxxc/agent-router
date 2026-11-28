//! Verify assignment generations across concurrent namespace changes.
use crate::workflow::{assignment, decode_record};
use anyhow::Result;
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::{fs, os::unix::fs::MetadataExt};
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    volume: u32,
    file: (u32, u32),
    attributes: u32,
    written: (u32, u32),
    len: u64,
}

#[cfg(windows)]
impl Stamp {
    fn from_file(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            volume: info.dwVolumeSerialNumber,
            file: (info.nFileIndexHigh, info.nFileIndexLow),
            attributes: info.dwFileAttributes,
            written: (
                info.ftLastWriteTime.dwHighDateTime,
                info.ftLastWriteTime.dwLowDateTime,
            ),
            len: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        })
    }

    pub(super) fn at(path: &Path) -> io::Result<Option<Self>> {
        // Query the object even if a content handle denies shared reads.
        use std::os::windows::fs::OpenOptionsExt;
        use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
        let file = match OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | crate::windows_process::OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Self::from_file(&file).map(Some)
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    device: u64,
    inode: u64,
    mode: u32,
    changed: (i64, i64),
    len: u64,
}

#[cfg(target_os = "linux")]
impl Stamp {
    fn new(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            len: metadata.len(),
        }
    }

    pub(super) fn at(path: &Path) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(Self::new(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn open_content(path: &Path) -> io::Result<(File, Stamp)> {
    let file = super::open_relay_file(OpenOptions::new().read(true)).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid dispatch generation",
        ));
    }
    #[cfg(windows)]
    let stamp = {
        use std::os::windows::fs::MetadataExt;
        use winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid dispatch generation reparse point",
            ));
        }
        Stamp::from_file(&file)?
    };
    #[cfg(target_os = "linux")]
    let stamp = Stamp::new(&metadata);
    Ok((file, stamp))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GenerationStamp {
    object: Stamp,
    // Sharing can change without an object stamp changing.
    #[cfg(windows)]
    readable: bool,
}

impl GenerationStamp {
    fn at(path: &Path) -> io::Result<Option<Self>> {
        let Some(object) = Stamp::at(path)? else {
            return Ok(None);
        };
        #[cfg(windows)]
        let readable = match open_content(path) {
            Ok((_, stamp)) if stamp == object => true,
            Ok(_) => return Err(io::Error::other("dispatch generation changed")),
            Err(_) => false,
        };
        Ok(Some(Self {
            object,
            #[cfg(windows)]
            readable,
        }))
    }
}

struct Generation {
    row: Option<Value>,
    stamp: GenerationStamp,
}

impl Generation {
    fn read(path: &Path) -> Result<Option<Self>> {
        let before = Stamp::at(path)?;
        let read = Self::read_content(path);
        if !matches!(&read, Ok(Some(_))) {
            if let Some(object) = before {
                anyhow::ensure!(Stamp::at(path)? == before, "dispatch generation changed");
                // A stable path can fence the namespace without authorizing a row.
                return Ok(Some(Self {
                    row: None,
                    stamp: GenerationStamp {
                        object,
                        #[cfg(windows)]
                        readable: false,
                    },
                }));
            }
        }
        read
    }

    fn read_content(path: &Path) -> Result<Option<Self>> {
        let (file, object) = match open_content(path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Self {
            stamp: GenerationStamp {
                object,
                #[cfg(windows)]
                readable: true,
            },
            // The relay needs the file identity even if the record fails to decode.
            row: decode_record(file, path).ok(),
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct GenerationStamps {
    pending: Option<GenerationStamp>,
    published: Option<GenerationStamp>,
}

impl GenerationStamps {
    pub(super) fn at(dir: &Path, id: &str) -> io::Result<Self> {
        Ok(Self {
            pending: GenerationStamp::at(&dir.join(format!("{id}.pending")))?,
            published: GenerationStamp::at(&dir.join(format!("{id}.json")))?,
        })
    }

    pub(super) fn current(self, dir: &Path, id: &str) -> bool {
        Self::at(dir, id).is_ok_and(|stamp| stamp == self)
    }
}

pub(super) struct Generations {
    pending: Option<Generation>,
    published: Option<Generation>,
    pub(super) damaged: bool,
}

impl Generations {
    pub(super) fn read(dir: &Path, id: &str) -> Self {
        // Read the reservation first to cover a concurrent promotion.
        let pending = Generation::read(&dir.join(format!("{id}.pending")));
        let published = Generation::read(&dir.join(format!("{id}.json")));
        let mut damaged = pending.is_err() || published.is_err();
        let pending = pending.ok().flatten();
        let published = published.ok().flatten();
        damaged |= pending.as_ref().is_some_and(|g| {
            g.row
                .as_ref()
                .is_none_or(|row| !assignment::valid_reservation(row))
        });
        damaged |= published.as_ref().is_some_and(|g| g.row.is_none());
        if let (Some(a), Some(b)) = (
            pending.as_ref().and_then(|g| g.row.as_ref()),
            published.as_ref().and_then(|g| g.row.as_ref()),
        ) {
            damaged |= !assignment::same_publication_identity(a, b);
        }
        Self {
            pending,
            published,
            damaged,
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    pub(super) fn rows(&self) -> impl Iterator<Item = &Value> {
        self.pending
            .iter()
            .chain(&self.published)
            .filter_map(|g| g.row.as_ref())
    }

    pub(super) fn assigned(&self) -> Option<&Value> {
        if self.damaged {
            return None;
        }
        self.published
            .as_ref()
            .or(self.pending.as_ref())
            .and_then(|g| g.row.as_ref())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn into_assigned(self) -> Result<Option<Value>> {
        anyhow::ensure!(!self.damaged, "{}", assignment::UNVERIFIED_ASSIGNMENT);
        if let Some(published) = self.published {
            return Ok(published.row);
        }
        Ok(self.pending.and_then(|g| g.row).map(|mut row| {
            row["createPending"] = Value::Bool(true);
            row
        }))
    }

    pub(super) fn message_boundary(&self) -> assignment::MessageBoundary {
        if self.damaged {
            return assignment::MessageBoundary::Unverified;
        }
        self.assigned()
            .map(|row| assignment::MessageBoundary::for_record(row, self.published.is_some()))
            .unwrap_or_default()
    }

    pub(super) fn stamps(&self) -> GenerationStamps {
        GenerationStamps {
            pending: self.pending.as_ref().map(|g| g.stamp),
            published: self.published.as_ref().map(|g| g.stamp),
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn pending(&self) -> Option<&Value> {
        self.pending
            .as_ref()
            .filter(|_| !self.damaged)
            .and_then(|g| g.row.as_ref())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn published(&self) -> Option<&Value> {
        self.published
            .as_ref()
            .filter(|_| !self.damaged)
            .and_then(|g| g.row.as_ref())
    }
}
