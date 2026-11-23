//! Newline-delimited JSON, with a size and a wall-clock budget for each frame.
use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;

pub(super) trait Socket: Read + Write {
    fn read_budget(&self, timeout: Duration) -> io::Result<()>;
    fn write_budget(&self, timeout: Duration) -> io::Result<()>;
}
macro_rules! socket {
    ($s:ty) => {
        impl Socket for $s {
            fn read_budget(&self, timeout: Duration) -> io::Result<()> {
                self.set_read_timeout(Some(timeout))
            }
            fn write_budget(&self, timeout: Duration) -> io::Result<()> {
                self.set_write_timeout(Some(timeout))
            }
        }
    };
}
socket!(UnixStream);
socket!(TcpStream);

pub(super) struct Wire<S>(BufReader<S>);
pub(super) const LIMIT: usize = 2 * 1024 * 1024;
pub(super) const PREAUTH: Duration = Duration::from_millis(500);
pub(super) const FRAME: Duration = Duration::from_secs(2);

fn remaining(end: Instant) -> Result<Duration> {
    let left = end.saturating_duration_since(Instant::now());
    ensure!(!left.is_zero(), "frame deadline");
    Ok(left)
}

impl<S: Socket> Wire<S> {
    pub fn new(socket: S) -> Self {
        Self(BufReader::new(socket))
    }

    pub fn read(&mut self, limit: usize, deadline: Instant) -> Result<Value> {
        let mut frame = Vec::new();
        loop {
            self.0.get_ref().read_budget(remaining(deadline)?)?;
            let buf = self.0.fill_buf()?;
            ensure!(!buf.is_empty(), "missing frame");
            let newline = buf.iter().position(|b| *b == b'\n');
            let n = newline.map_or(buf.len(), |i| i + 1);
            ensure!(frame.len() + n <= limit, "oversized frame");
            frame.extend_from_slice(&buf[..n]);
            self.0.consume(n);
            if newline.is_some() {
                return Ok(serde_json::from_slice(&frame)?);
            }
        }
    }

    pub fn write(&mut self, value: &Value, deadline: Instant) -> Result<()> {
        self.write_bytes(&encode(value)?, deadline)
    }

    pub fn write_bytes(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<()> {
        let stream = self.0.get_mut();
        while !bytes.is_empty() {
            stream.write_budget(remaining(deadline)?)?;
            let n = stream.write(bytes)?;
            ensure!(n != 0, "write closed");
            bytes = &bytes[n..];
        }
        Ok(())
    }
}

pub fn encode(v: &Value) -> Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(v)?;
    frame.push(b'\n');
    ensure!(frame.len() <= LIMIT, "oversized frame");
    Ok(frame)
}
