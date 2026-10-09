//! The host end of the guest control protocol: a connection to the guest agent through
//! Firecracker's vsock proxy socket (`CONNECT 5200` / `OK <n>`), or through the fake guest,
//! which speaks the same handshake. Blocking I/O with deadlines; the worker runs it on its
//! own thread of control.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{
    FILE_LIMIT, Frame, FrameError, GUEST_PROTOCOL, Message, PATCH_LIMIT, RAW_FRAME_LIMIT,
    SNAPSHOT_BYTES_LIMIT, SNAPSHOT_FILES_LIMIT, VSOCK_PORT, raw_frames_for, read_frame,
    write_frame,
};
use agentos_core::workspace::list_files;
use serde::{Deserialize, Serialize};

/// How the worker reaches a guest: the real Firecracker binary, or (tests only) a program
/// that runs `agentos_guest::fake::serve`, invoked as `program prefix_args… fake-guest UDS ROOT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestLauncher {
    Real {
        firecracker_bin: PathBuf,
    },
    Fake {
        program: PathBuf,
        prefix_args: Vec<String>,
    },
}

#[derive(Debug)]
pub enum LinkError {
    BootTimeout,
    /// The guest's process ended before a connection was made; the text says how.
    Exited(String),
    Lost(io::Error),
    Protocol(String),
    Refused(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::BootTimeout => {
                f.write_str("guest did not come up: no connection before the boot deadline")
            }
            LinkError::Exited(how) => write!(f, "guest did not come up: {how}"),
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
/// The longest path a Unix socket address holds (`sun_path` is 108 bytes with the NUL).
const SUN_PATH_MAX: usize = 107;

/// A path that reaches the socket `uds` within `SUN_PATH_MAX`: `uds` itself, or, when it is
/// longer (a job directory is named `<64-hex effect>-<uuid>`), `/proc/self/fd/<n>/<name>`
/// through a descriptor of its directory, which the returned `File` keeps open.
pub fn socket_path(uds: &Path) -> io::Result<(PathBuf, Option<File>)> {
    if uds.as_os_str().len() <= SUN_PATH_MAX {
        return Ok((uds.to_path_buf(), None));
    }
    let (Some(dir), Some(name)) = (uds.parent(), uds.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a socket path", uds.display()),
        ));
    };
    let dir = File::open(dir)?;
    let alias = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
    Ok((alias, Some(dir)))
}

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
        FrameError::Io(e) => LinkError::Lost(normalize(e)),
        // A JSON error quotes the guest's bytes (e.g. an unknown `type`).
        other => LinkError::Protocol(guest_text(&other.to_string())),
    }
}

/// At most this many bytes of guest-controlled text reach an error or a log line.
pub const GUEST_TEXT_LIMIT: usize = 512;

/// `s` with every control character (and the Unicode line/paragraph separators) escaped,
/// so it is always one line of text.
pub fn escape_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    out
}

/// Guest-controlled text made fit for an error that may end up in a log: control
/// characters escaped, cut (on a char boundary) to `GUEST_TEXT_LIMIT` bytes and marked.
pub fn guest_text(s: &str) -> String {
    let mut out = escape_controls(s);
    if out.len() > GUEST_TEXT_LIMIT {
        let mut end = GUEST_TEXT_LIMIT;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str(" [truncated]");
    }
    out
}

fn remaining(until: Instant) -> Duration {
    until
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

/// A stalled read surfaces as `TimedOut` with a plain message, not "Resource temporarily
/// unavailable".
fn normalize(e: io::Error) -> io::Error {
    if is_timeout(&e) {
        io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for the guest")
    } else {
        e
    }
}

/// A reader whose total wait is bounded: the socket timeout is re-armed before every read
/// from the time left until `until`, so a peer dripping bytes cannot stretch the deadline.
struct DeadlineReader<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(normalize(io::ErrorKind::TimedOut.into()));
        }
        self.stream.set_read_timeout(Some(remaining(self.until)))?;
        self.stream.read(buf).map_err(normalize)
    }
}

/// Reads the handshake reply a byte at a time (nothing of the first frame is consumed).
/// `Ok(None)` is a connection closed before a full line.
fn read_line(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= MAX_HANDSHAKE_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake reply too long",
            ));
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
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
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
        GuestLink::connect_until(uds, deadline, || None)
    }

    /// `connect`, asking `gone` whenever the socket cannot be reached whether the guest's
    /// process has ended (`Some(how)` ends the wait at once as `Exited(how)`). Only then: a
    /// dead process cannot be listening, while a reachable socket is always given the
    /// handshake up to `deadline`.
    pub fn connect_until(
        uds: &Path,
        deadline: Instant,
        mut gone: impl FnMut() -> Option<String>,
    ) -> Result<GuestLink, LinkError> {
        loop {
            if Instant::now() >= deadline {
                return Err(LinkError::BootTimeout);
            }
            let reached = socket_path(uds).and_then(|(path, _dir)| UnixStream::connect(path));
            let Ok(mut stream) = reached else {
                if let Some(how) = gone() {
                    return Err(LinkError::Exited(how));
                }
                thread::sleep(SOCKET_POLL);
                continue;
            };
            let reply = match stream.write_all(format!("CONNECT {VSOCK_PORT}\n").as_bytes()) {
                Ok(()) => read_line(&mut DeadlineReader {
                    stream: &stream,
                    until: deadline,
                }),
                Err(e) => Err(e),
            };
            match reply {
                Ok(Some(line)) => {
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if !text
                        .strip_prefix("OK ")
                        .is_some_and(|n| n.parse::<u32>().is_ok())
                    {
                        return Err(LinkError::Protocol(format!(
                            "unexpected handshake reply {text:?}"
                        )));
                    }
                    stream.set_read_timeout(None).map_err(lost)?;
                    return Ok(GuestLink { stream });
                }
                Ok(None) => thread::sleep(RETRY_PAUSE),
                Err(e) if is_timeout(&e) => return Err(LinkError::BootTimeout),
                Err(e) if matches!(e.kind(), io::ErrorKind::InvalidData) => {
                    return Err(LinkError::Protocol(e.to_string()));
                }
                Err(_) => thread::sleep(RETRY_PAUSE),
            }
        }
    }

    pub fn send(&mut self, msg: &Message) -> Result<(), LinkError> {
        write_frame(&mut self.stream, &Frame::Json(msg.clone())).map_err(lost)
    }

    /// Sends `bytes` as `raw_frames_for(len)` raw frames (every one full except the last);
    /// no bytes, no frames.
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        for i in 0..raw_frames_for(bytes.len() as u64) as usize {
            let chunk = &bytes[i * RAW_FRAME_LIMIT..bytes.len().min((i + 1) * RAW_FRAME_LIMIT)];
            write_frame(&mut self.stream, &Frame::Raw(chunk.to_vec())).map_err(lost)?;
        }
        Ok(())
    }

    /// Sends `bytes` as exactly one raw frame, empty included (the patch of `ApplyPatch` and
    /// `PatchState`, which the guest reads as one frame of at most `PATCH_LIMIT` bytes).
    pub fn send_patch(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        if bytes.len() > PATCH_LIMIT {
            return Err(LinkError::Protocol(format!(
                "patch of {} bytes, over the {PATCH_LIMIT} limit",
                bytes.len()
            )));
        }
        write_frame(&mut self.stream, &Frame::Raw(bytes.to_vec())).map_err(lost)
    }

    /// Sends `bytes` as exactly one raw frame, empty included, at most `RAW_FRAME_LIMIT` bytes:
    /// the body of a `ModelReply`, which the guest reads as one frame.
    pub fn send_body(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        if bytes.len() > RAW_FRAME_LIMIT {
            return Err(LinkError::Protocol(format!(
                "body of {} bytes, over the {RAW_FRAME_LIMIT} limit",
                bytes.len()
            )));
        }
        write_frame(&mut self.stream, &Frame::Raw(bytes.to_vec())).map_err(lost)
    }

    /// Bounds every single write on the connection (a peer that stops reading).
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), LinkError> {
        self.stream.set_write_timeout(timeout).map_err(lost)
    }

    /// The next message, waiting at most until `until`. A timeout is `Lost`; a raw frame
    /// where a message is expected is `Protocol`.
    pub fn recv(&mut self, until: Instant) -> Result<Message, LinkError> {
        match read_frame(
            &mut DeadlineReader {
                stream: &self.stream,
                until,
            },
            0,
        )
        .map_err(frame_error)?
        {
            Frame::Json(m) => Ok(m),
            Frame::Raw(_) => Err(LinkError::Protocol(
                "raw frame where a message was expected".into(),
            )),
        }
    }

    /// The next frame of any kind, once it has started: `Ok(None)` when nothing starts within
    /// `wait`, so a caller can poll other conditions between frames. Once the first byte is
    /// in, the frame is read whole, within `until`.
    pub fn poll_frame(
        &mut self,
        wait: Duration,
        until: Instant,
    ) -> Result<Option<Frame>, LinkError> {
        self.stream
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
            .map_err(lost)?;
        let mut first = [0u8; 1];
        loop {
            match self.stream.read(&mut first) {
                Ok(0) => return Err(lost(io::ErrorKind::UnexpectedEof.into())),
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if is_timeout(&e) => return Ok(None),
                Err(e) => return Err(lost(e)),
            }
        }
        let mut rest = (&first[..]).chain(DeadlineReader {
            stream: &self.stream,
            until,
        });
        read_frame(&mut rest, RAW_FRAME_LIMIT)
            .map(Some)
            .map_err(frame_error)
    }

    /// The next frame, of any kind, waiting at most until `until`.
    pub fn recv_frame(&mut self, until: Instant) -> Result<Frame, LinkError> {
        read_frame(
            &mut DeadlineReader {
                stream: &self.stream,
                until,
            },
            RAW_FRAME_LIMIT,
        )
        .map_err(frame_error)
    }

    /// The raw frame that must come next (the body after a `ModelRequest`, the patch after
    /// `AgentDone`): a message there is a protocol violation.
    pub fn recv_body(&mut self, until: Instant) -> Result<Vec<u8>, LinkError> {
        match self.recv_frame(until)? {
            Frame::Raw(bytes) => Ok(bytes),
            Frame::Json(m) => Err(LinkError::Protocol(format!(
                "{} where a raw frame was expected",
                type_name(&m)
            ))),
        }
    }

    /// Sends `Hello` and requires `Ready` at protocol 1.
    pub fn hello(&mut self, hello: Message, until: Instant) -> Result<Message, LinkError> {
        self.send(&hello)?;
        match self.recv(until)? {
            ready @ Message::Ready {
                protocol: GUEST_PROTOCOL,
                ..
            } => Ok(ready),
            Message::Ready { protocol, .. } => Err(LinkError::Protocol(format!(
                "guest speaks protocol {protocol}, expected {GUEST_PROTOCOL}"
            ))),
            Message::Refused { reason } => Err(LinkError::Refused(reason)),
            other => Err(LinkError::Protocol(format!(
                "expected Ready, got {}",
                type_name(&other)
            ))),
        }
    }

    /// Streams every regular file under `root` as `File{path,len}` + raw frames (sorted),
    /// then `EndFiles`. Returns `(files, bytes)`. The limits are checked before anything
    /// is written. These are the *snapshot* limits (256 MiB, 65 536 files); a profile
    /// (`RunVerification`) is bounded by `PROFILE_LIMIT` (64 MiB), which the caller checks
    /// with `count_tree` before sending the request. The caller must have sent the request
    /// that announces the stream (`ReadSnapshot`/`RunVerification` with the counts from
    /// `count_tree`) first. Any error from a link means the stream may be desynchronised:
    /// drop the link, never reuse it.
    pub fn send_tree(&mut self, root: &Path) -> Result<(u64, u64), LinkError> {
        let entries = scan(root)
            .map_err(|e| LinkError::Protocol(format!("cannot read {}: {e}", root.display())))?;
        let files = entries.len() as u64;
        let bytes: u64 = entries.iter().map(|e| e.len).sum();
        if files > SNAPSHOT_FILES_LIMIT {
            return Err(LinkError::Protocol(format!(
                "tree has {files} files, over the {SNAPSHOT_FILES_LIMIT} limit"
            )));
        }
        if bytes > SNAPSHOT_BYTES_LIMIT {
            return Err(LinkError::Protocol(format!(
                "tree has {bytes} bytes, over the {SNAPSHOT_BYTES_LIMIT} limit"
            )));
        }
        if let Some(big) = entries.iter().find(|e| e.len > FILE_LIMIT) {
            return Err(LinkError::Protocol(format!(
                "file {} is {} bytes, over the {FILE_LIMIT} limit",
                big.rel, big.len
            )));
        }
        for e in &entries {
            self.send(&Message::File {
                path: e.rel.clone(),
                len: e.len,
            })?;
            let mut file = File::open(&e.path)
                .map_err(|err| LinkError::Protocol(format!("cannot read {}: {err}", e.rel)))?;
            for i in 0..raw_frames_for(e.len) {
                let size =
                    (e.len - i * RAW_FRAME_LIMIT as u64).min(RAW_FRAME_LIMIT as u64) as usize;
                let mut chunk = vec![0u8; size];
                file.read_exact(&mut chunk).map_err(|err| {
                    LinkError::Protocol(format!("{} changed while it was sent: {err}", e.rel))
                })?;
                write_frame(&mut self.stream, &Frame::Raw(chunk)).map_err(lost)?;
            }
        }
        self.send(&Message::EndFiles)?;
        Ok((files, bytes))
    }

    /// `(files, bytes)` of the tree (no limit is enforced here), for the `file_count`/`total_bytes` of the request
    /// that precedes `send_tree`.
    pub fn count_tree(root: &Path) -> io::Result<(u64, u64)> {
        let entries = scan(root)?;
        Ok((entries.len() as u64, entries.iter().map(|e| e.len).sum()))
    }
}

/// The `type` tag of a message, for error texts.
pub(crate) fn type_name(m: &Message) -> String {
    serde_json::to_value(m)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_string)))
        .unwrap_or_else(|| "?".into())
}

/// Starts the fake guest as `program prefix_args… fake-guest NAME ROOT` with null stdio, in
/// the caller's process group (as Firecracker will be), in the socket's directory with the
/// socket's file name (as Firecracker binds its relative `uds_path`, so a long directory
/// never exceeds the socket address limit). The environment is cleared apart from `PATH`
/// and `env`. The caller kills and reaps the child explicitly.
pub fn spawn_fake(
    launcher: &GuestLauncher,
    uds: &Path,
    root: &Path,
    env: &[(String, String)],
) -> io::Result<Child> {
    fake_command(launcher, uds, root, env)?.spawn()
}

/// The command `spawn_fake` spawns, for a caller that adds to it (the inspector puts the
/// fake guest in a process group of its own).
pub fn fake_command(
    launcher: &GuestLauncher,
    uds: &Path,
    root: &Path,
    env: &[(String, String)],
) -> io::Result<Command> {
    let GuestLauncher::Fake {
        program,
        prefix_args,
    } = launcher
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn_fake needs a Fake launcher",
        ));
    };
    let mut cmd = Command::new(program);
    match (
        uds.parent().filter(|d| !d.as_os_str().is_empty()),
        uds.file_name(),
    ) {
        (Some(dir), Some(name)) => cmd
            .current_dir(dir)
            .args(prefix_args)
            .arg("fake-guest")
            .arg(name)
            .arg(root),
        _ => cmd.args(prefix_args).arg("fake-guest").arg(uds).arg(root),
    };
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(cmd)
}
