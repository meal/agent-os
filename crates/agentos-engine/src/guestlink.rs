//! The host end of the guest control protocol: a connection to the guest agent through
//! Firecracker's vsock proxy socket (`CONNECT 5200` / `OK <n>`), or through the fake guest,
//! which speaks the same handshake. Blocking I/O with deadlines; the worker runs it on its
//! own thread of control.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{
    read_frame, write_frame, Frame, FrameError, Message, FILE_LIMIT, GUEST_PROTOCOL, RAW_FRAME_LIMIT, SNAPSHOT_BYTES_LIMIT,
    SNAPSHOT_FILES_LIMIT, VSOCK_PORT,
};
use agentos_core::workspace::list_files;
use serde::{Deserialize, Serialize};

/// How the worker reaches a guest: the real Firecracker binary, or (tests only) a program
/// that runs `agentos_guest::fake::serve`, invoked as `program prefix_args… fake-guest UDS ROOT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestLauncher {
    Real { firecracker_bin: PathBuf },
    Fake { program: PathBuf, prefix_args: Vec<String> },
}

#[derive(Debug)]
pub enum LinkError {
    BootTimeout,
    Lost(io::Error),
    Protocol(String),
    Refused(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::BootTimeout => f.write_str("guest did not come up: no connection before the boot deadline"),
            LinkError::Lost(e) => write!(f, "guest connection lost: {e}"),
            LinkError::Protocol(why) => write!(f, "guest protocol violation: {why}"),
            LinkError::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for LinkError {}

const SOCKET_POLL: Duration = Duration::from_millis(10);
const RETRY_PAUSE: Duration = Duration::from_millis(50);
const MAX_HANDSHAKE_LINE: usize = 64;

pub struct GuestLink {
    stream: UnixStream,
}

fn lost(e: io::Error) -> LinkError {
    LinkError::Lost(e)
}

/// Frame-level failures: a closed or stalled connection is a loss, anything the peer got
/// wrong is a protocol violation.
fn frame_error(e: FrameError) -> LinkError {
    match e {
        FrameError::Io(e) => LinkError::Lost(e),
        other => LinkError::Protocol(other.to_string()),
    }
}

fn remaining(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))
}

/// Reads the handshake reply a byte at a time (nothing of the first frame is consumed).
/// `Ok(None)` is a connection closed before a full line.
fn read_line(stream: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= MAX_HANDSHAKE_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "handshake reply too long"));
        }
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(line))
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

struct Entry {
    rel: String,
    path: PathBuf,
    len: u64,
}

/// Sorted regular files under `root` with their sizes.
fn scan(root: &Path) -> io::Result<Vec<Entry>> {
    list_files(root)?
        .into_iter()
        .map(|(rel, path)| {
            let len = path.metadata()?.len();
            Ok(Entry { rel, path, len })
        })
        .collect()
}

impl GuestLink {
    /// Waits for the guest's socket (polling every 10 ms), connects and performs the
    /// handshake. A connection the proxy closes because the guest is not listening yet is
    /// retried every 50 ms; past `deadline` it is `BootTimeout`.
    pub fn connect(uds: &Path, deadline: Instant) -> Result<GuestLink, LinkError> {
        let expected = format!("OK {VSOCK_PORT}");
        loop {
            if Instant::now() >= deadline {
                return Err(LinkError::BootTimeout);
            }
            let Ok(mut stream) = UnixStream::connect(uds) else {
                thread::sleep(SOCKET_POLL);
                continue;
            };
            let _ = stream.set_read_timeout(Some(remaining(deadline)));
            let reply = match stream.write_all(format!("CONNECT {VSOCK_PORT}\n").as_bytes()) {
                Ok(()) => read_line(&mut stream),
                Err(e) => Err(e),
            };
            match reply {
                Ok(Some(line)) => {
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if !(text == expected || text.starts_with("OK ")) {
                        return Err(LinkError::Protocol(format!("unexpected handshake reply {text:?}")));
                    }
                    stream.set_read_timeout(None).map_err(lost)?;
                    return Ok(GuestLink { stream });
                }
                Ok(None) => thread::sleep(RETRY_PAUSE),
                Err(e) if is_timeout(&e) => return Err(LinkError::BootTimeout),
                Err(e) if matches!(e.kind(), io::ErrorKind::InvalidData) => return Err(LinkError::Protocol(e.to_string())),
                Err(_) => thread::sleep(RETRY_PAUSE),
            }
        }
    }

    pub fn send(&mut self, msg: &Message) -> Result<(), LinkError> {
        write_frame(&mut self.stream, &Frame::Json(msg.clone())).map_err(lost)
    }

    /// Sends `bytes` as raw frames of at most `RAW_FRAME_LIMIT`; no bytes, no frames.
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        for chunk in bytes.chunks(RAW_FRAME_LIMIT) {
            write_frame(&mut self.stream, &Frame::Raw(chunk.to_vec())).map_err(lost)?;
        }
        Ok(())
    }

    /// The next message, waiting at most until `until`. A timeout is `Lost`; a raw frame
    /// where a message is expected is `Protocol`.
    pub fn recv(&mut self, until: Instant) -> Result<Message, LinkError> {
        self.stream.set_read_timeout(Some(remaining(until))).map_err(lost)?;
        match read_frame(&mut self.stream, 0).map_err(frame_error)? {
            Frame::Json(m) => Ok(m),
            Frame::Raw(_) => Err(LinkError::Protocol("raw frame where a message was expected".into())),
        }
    }

    /// Sends `Hello` and requires `Ready` at protocol 1.
    pub fn hello(&mut self, hello: Message, until: Instant) -> Result<Message, LinkError> {
        self.send(&hello)?;
        match self.recv(until)? {
            ready @ Message::Ready { protocol: GUEST_PROTOCOL, .. } => Ok(ready),
            Message::Ready { protocol, .. } => Err(LinkError::Protocol(format!("guest speaks protocol {protocol}, expected {GUEST_PROTOCOL}"))),
            Message::Refused { reason } => Err(LinkError::Refused(reason)),
            other => Err(LinkError::Protocol(format!("expected Ready, got {}", type_name(&other)))),
        }
    }

    /// Streams every regular file under `root` as `File{path,len}` + raw frames (sorted),
    /// then `EndFiles`. Returns `(files, bytes)`. The limits are checked before anything
    /// is written.
    pub fn send_tree(&mut self, root: &Path) -> Result<(u64, u64), LinkError> {
        let entries = scan(root).map_err(|e| LinkError::Protocol(format!("cannot read {}: {e}", root.display())))?;
        let files = entries.len() as u64;
        let bytes: u64 = entries.iter().map(|e| e.len).sum();
        if files > SNAPSHOT_FILES_LIMIT {
            return Err(LinkError::Protocol(format!("tree has {files} files, over the {SNAPSHOT_FILES_LIMIT} limit")));
        }
        if bytes > SNAPSHOT_BYTES_LIMIT {
            return Err(LinkError::Protocol(format!("tree has {bytes} bytes, over the {SNAPSHOT_BYTES_LIMIT} limit")));
        }
        if let Some(big) = entries.iter().find(|e| e.len > FILE_LIMIT) {
            return Err(LinkError::Protocol(format!("file {} is {} bytes, over the {FILE_LIMIT} limit", big.rel, big.len)));
        }
        for e in &entries {
            self.send(&Message::File { path: e.rel.clone(), len: e.len })?;
            let mut file = File::open(&e.path).map_err(|err| LinkError::Protocol(format!("cannot read {}: {err}", e.rel)))?;
            let mut left = e.len;
            while left > 0 {
                let mut chunk = vec![0u8; left.min(RAW_FRAME_LIMIT as u64) as usize];
                file.read_exact(&mut chunk).map_err(|err| LinkError::Protocol(format!("{} changed while it was sent: {err}", e.rel)))?;
                write_frame(&mut self.stream, &Frame::Raw(chunk)).map_err(lost)?;
                left -= left.min(RAW_FRAME_LIMIT as u64);
            }
        }
        self.send(&Message::EndFiles)?;
        Ok((files, bytes))
    }

    /// `(files, bytes)` of the tree, for the `file_count`/`total_bytes` of the request
    /// that precedes `send_tree`.
    pub fn count_tree(root: &Path) -> io::Result<(u64, u64)> {
        let entries = scan(root)?;
        Ok((entries.len() as u64, entries.iter().map(|e| e.len).sum()))
    }
}

fn type_name(m: &Message) -> String {
    serde_json::to_value(m)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
        .unwrap_or_else(|| "?".into())
}

/// Starts the fake guest as `program prefix_args… fake-guest UDS ROOT` with null stdio, in
/// the caller's process group (as Firecracker will be). The environment is cleared apart
/// from `PATH` and `env`. The caller kills and reaps the child explicitly.
pub fn spawn_fake(launcher: &GuestLauncher, uds: &Path, root: &Path, env: &[(String, String)]) -> io::Result<Child> {
    let GuestLauncher::Fake { program, prefix_args } = launcher else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "spawn_fake needs a Fake launcher"));
    };
    let mut cmd = Command::new(program);
    cmd.args(prefix_args).arg("fake-guest").arg(uds).arg(root);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
}
