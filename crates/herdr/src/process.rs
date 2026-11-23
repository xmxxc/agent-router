use super::{Failure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::ErrorKind};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub creation: String,
}

impl Process {
    // None is positive evidence of exit. Permission, parse and procfs errors stay unknown.
    pub(super) fn observe(pid: u32) -> Result<Option<Self>> {
        let unknown = Failure::unknown("cannot observe Linux process creation identity");
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|_| unknown)?;
        if uuid::Uuid::parse_str(boot.trim()).is_err() || pid == 0 {
            return Err(unknown);
        }
        let stat = match fs::read(format!("/proc/{pid}/stat")) {
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            other => other.map_err(|_| unknown)?,
        };
        // comm may contain spaces, ')' or non-UTF8 bytes. Field 22 counts from the last ')'.
        let end = stat.iter().rposition(|b| *b == b')').ok_or(unknown)?;
        let mut fields = std::str::from_utf8(&stat[end + 1..])
            .map_err(|_| unknown)?
            .split_whitespace();
        if matches!(fields.next(), Some("Z" | "X" | "x")) {
            return Ok(None);
        }
        let ticks: u64 = fields.nth(18).and_then(|s| s.parse().ok()).ok_or(unknown)?;
        Ok(Some(Self {
            pid,
            creation: format!("linux:{}:{ticks}", boot.trim()),
        }))
    }

    pub(super) fn alive(&self) -> Result<bool> {
        Ok(Self::observe(self.pid)?.as_ref() == Some(self))
    }
}

pub(super) fn writers(info: &Value, pane: &str) -> Result<Vec<Process>> {
    let unknown = Failure::unknown("partial Herdr writer identity");
    if info["pane_id"] != pane {
        return Err(unknown);
    }
    let pid = |v: &Value| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or(unknown)
    };
    let mut pids = vec![pid(&info["shell_pid"])?];
    if let Some(group) = info.get("foreground_process_group_id") {
        pid(group)?;
        if info["foreground_processes"]
            .as_array()
            .is_none_or(Vec::is_empty)
        {
            return Err(unknown);
        }
    }
    if let Some(foreground) = info.get("foreground_processes") {
        for entry in foreground.as_array().ok_or(unknown)? {
            let id = pid(&entry["pid"])?;
            if !pids.contains(&id) {
                pids.push(id);
            }
        }
    }
    pids.into_iter()
        .map(|id| Process::observe(id)?.ok_or(unknown))
        .collect()
}
