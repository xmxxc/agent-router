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
    /// None is positive evidence of exit. Permission, parse and procfs errors stay unknown.
    pub fn observe(pid: u32) -> Result<Option<Self>> {
        Self::read(pid, false)
    }

    /// Read creation identity before waiting on an owned child. An exited,
    /// unreaped child still owns its PID and its procfs start time.
    pub fn observe_unreaped_child(child: &std::process::Child) -> Result<Option<Self>> {
        let unknown = Failure::unknown("cannot verify Linux child PID namespace");
        let own_pid = fs::read_link("/proc/self").map_err(|_| unknown)?;
        if own_pid.to_str().and_then(|pid| pid.parse::<u32>().ok()) != Some(std::process::id()) {
            return Err(unknown);
        }
        Self::read(child.id(), true)
    }

    fn read(pid: u32, include_exited: bool) -> Result<Option<Self>> {
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
        let state = fields.next().ok_or(unknown)?;
        if !include_exited && matches!(state, "Z" | "X" | "x") {
            return Ok(None);
        }
        let ticks: u64 = fields.nth(18).and_then(|s| s.parse().ok()).ok_or(unknown)?;
        Ok(Some(Self {
            pid,
            creation: format!("linux:{}:{ticks}", boot.trim()),
        }))
    }

    pub fn alive(&self) -> Result<bool> {
        Ok(Self::observe(self.pid)?.as_ref() == Some(self))
    }
}

pub(super) struct Writers {
    // The first entry always has the observed identity of the PTY root.
    pub known: Vec<Process>,
    pub completeness: Result<()>,
}

pub(super) fn writers(info: &Value, pane: &str) -> Result<Writers> {
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
    let identity = |v: &Value| Process::observe(pid(v)?)?.ok_or(unknown);
    let root = identity(&info["shell_pid"])?;
    let mut known = vec![root];
    let members = info
        .get("foreground_processes")
        .map(|v| v.as_array().ok_or(unknown))
        .transpose()?;
    let mut completeness = Ok(());
    if let Some(group) = info.get("foreground_process_group_id") {
        if pid(group).is_err() || members.is_none_or(Vec::is_empty) {
            completeness = Err(unknown);
        }
    }
    for member in members.into_iter().flatten() {
        match identity(&member["pid"]) {
            Ok(writer) => {
                if !known.contains(&writer) {
                    known.push(writer);
                }
            }
            Err(err) => completeness = Err(err),
        }
    }
    Ok(Writers {
        known,
        completeness,
    })
}
