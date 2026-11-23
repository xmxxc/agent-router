//! Herdr consumes one newline-delimited JSON request per connection.
use super::{Failure, Process, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

const LIMIT: usize = 2 * 1024 * 1024;

// A blocking connect on a full Unix listen queue ignores the read/write deadlines.
fn connect(path: &Path) -> Result<(UnixStream, Process)> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is a plain C struct. Zeroing also terminates sun_path.
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    if !path.is_absolute() || bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
        return Err(Failure::refused("invalid Herdr socket path"));
    }
    let metadata = || {
        let m =
            fs::symlink_metadata(path).map_err(|_| Failure::unsent("Herdr socket unavailable"))?;
        if !m.file_type().is_socket()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
        {
            return Err(Failure::refused(
                "Herdr requires an owner-only local socket",
            ));
        }
        Ok((m.dev(), m.ino()))
    };
    let before = metadata()?;
    addr.sun_family = libc::AF_UNIX as _;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    // SAFETY: take sole ownership of a valid new fd, then pass a fully initialized address.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(Failure::unsent("cannot open Herdr connection"));
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let connected =
        unsafe { libc::connect(fd, (&raw const addr).cast(), mem::size_of_val(&addr) as _) };
    if connected != 0 {
        return Err(Failure::unsent("cannot connect to Herdr"));
    }
    if metadata()? != before {
        return Err(Failure::refused("Herdr socket changed during connect"));
    }
    stream
        .set_nonblocking(false)
        .map_err(|_| Failure::unsent("cannot configure Herdr connection"))?;
    // SAFETY: getsockopt initializes at most len bytes of ucred. Verify the actual length.
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of_val(&cred) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0
        || len as usize != mem::size_of_val(&cred)
        || cred.uid != unsafe { libc::geteuid() }
        || cred.pid <= 0
    {
        return Err(Failure::refused(
            "Herdr peer credentials unavailable or mismatched",
        ));
    }
    let peer =
        Process::observe(cred.pid as u32)?.ok_or(Failure::unknown("Herdr peer disappeared"))?;
    Ok((stream, peer))
}

pub(super) struct Session {
    endpoint: PathBuf,
    pub service: Process,
    deadline: Instant,
}

impl Session {
    pub fn open(endpoint: &Path, expected: Option<&Process>) -> Result<Self> {
        let seconds = match std::env::var("WFC_HERDR_TIMEOUT") {
            Ok(value) => value.parse::<f64>().ok(),
            Err(std::env::VarError::NotPresent) => Some(8.0),
            Err(_) => None,
        }
        .filter(|n| *n > 0. && *n <= 60.)
        .ok_or(Failure::refused("WFC_HERDR_TIMEOUT must be in (0, 60]"))?;
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        let (socket, service) = connect(endpoint)?;
        if expected.is_some_and(|old| old != &service) {
            return Err(Failure::unknown("Herdr service identity changed"));
        }
        let rpc = Self {
            endpoint: endpoint.into(),
            service,
            deadline,
        };
        let pong = rpc.exchange(socket, "ping", json!({}))?;
        if pong["version"] != "0.9.5" || pong["protocol"] != 22 {
            return Err(Failure::refused(
                "Herdr requires version 0.9.5 / protocol 22",
            ));
        }
        Ok(rpc)
    }

    pub fn snapshot(&self) -> Result<Value> {
        let mut result = self.call("session.snapshot", json!({}))?;
        if !result["snapshot"].is_object() {
            return Err(super::INVALID);
        }
        Ok(result["snapshot"].take())
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let before_send = |e: Failure| {
            if method == "pane.close" {
                e.before_effect()
            } else {
                e
            }
        };
        let (socket, peer) = connect(&self.endpoint).map_err(before_send)?;
        if peer != self.service {
            return Err(before_send(Failure::unknown(
                "Herdr service identity changed",
            )));
        }
        self.exchange(socket, method, params)
    }

    fn exchange(&self, mut socket: UnixStream, method: &str, params: Value) -> Result<Value> {
        let id = Uuid::new_v4().simple().to_string();
        let mut frame =
            serde_json::to_vec(&json!({"id":id, "method":method, "params":params})).unwrap();
        frame.push(b'\n');
        if frame.len() > LIMIT {
            return Err(Failure::refused("Herdr request exceeds limit"));
        }
        let unknown = Failure::unknown("Herdr exchange incomplete or invalid");
        let remaining = || {
            self.deadline
                .checked_duration_since(Instant::now())
                .filter(|t| !t.is_zero())
                .ok_or(unknown)
        };
        let mut bytes = frame.as_slice();
        while !bytes.is_empty() {
            let write_failure = if method == "pane.close" && bytes.len() == frame.len() {
                Failure::unsent("Herdr close was not sent")
            } else {
                unknown
            };
            socket
                .set_write_timeout(Some(remaining().map_err(|_| write_failure)?))
                .map_err(|_| write_failure)?;
            let n = socket.write(bytes).map_err(|_| write_failure)?;
            if n == 0 {
                return Err(write_failure);
            }
            bytes = &bytes[n..];
        }
        frame.clear();
        let mut chunk = [0; 8192];
        loop {
            socket
                .set_read_timeout(Some(remaining()?))
                .map_err(|_| unknown)?;
            let n = socket.read(&mut chunk).map_err(|_| unknown)?;
            if n == 0 || frame.len() + n > LIMIT {
                return Err(unknown);
            }
            frame.extend_from_slice(&chunk[..n]);
            if chunk[..n].contains(&b'\n') {
                break;
            }
        }
        let response: Value = serde_json::from_slice(&frame).map_err(|_| unknown)?;
        if frame.iter().position(|c| *c == b'\n') != Some(frame.len() - 1) || response["id"] != id {
            return Err(unknown);
        }
        if let Some(error) = response.get("error") {
            if response.get("result").is_some() {
                return Err(unknown);
            }
            return Err(match error["code"].as_str() {
                Some(
                    "invalid_request"
                    | "invalid_target"
                    | "invalid_key"
                    | "pane_not_found"
                    | "workspace_not_found"
                    | "permission_denied"
                    | "confirmation_required",
                ) => Failure::refused("Herdr rejected request"),
                _ => unknown,
            });
        }
        let result_type = match method {
            "ping" => "pong",
            "session.snapshot" => "session_snapshot",
            "workspace.create" => "workspace_created",
            "layout.apply" => "layout_apply",
            "pane.read" => "pane_read",
            "pane.process_info" => "pane_process_info",
            "workspace.report_metadata" | "pane.send_input" | "pane.send_keys" | "pane.close" => {
                "ok"
            }
            _ => return Err(unknown),
        };
        response
            .get("result")
            .filter(|v| v["type"] == result_type)
            .cloned()
            .ok_or(unknown)
    }
}
