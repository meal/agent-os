//! Guest control protocol: messages, limits, frame codec, attempt tokens.
//! Pure wire format shared by the host link and the guest agent; no I/O beyond `Read`/`Write`.

use std::fmt;
use std::io::{self, Read, Write};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::ids::Digest;

pub const GUEST_PROTOCOL: u32 = 2;
pub const VSOCK_PORT: u32 = 5200;
pub const GUEST_CID: u32 = 3;
pub const JSON_FRAME_LIMIT: usize = 1 << 20;
pub const RAW_FRAME_LIMIT: usize = 16 << 20;
pub const FILE_LIMIT: u64 = 64 << 20;
pub const SNAPSHOT_BYTES_LIMIT: u64 = 256 << 20;
pub const SNAPSHOT_FILES_LIMIT: u64 = 65_536;
pub const PROFILE_LIMIT: u64 = 64 << 20;
pub const PATCH_LIMIT: usize = 4 << 20;
pub const OUTPUT_LIMIT: usize = 64 * 1024;
pub const WS_IMAGE_BYTES: u64 = 1 << 30;
pub const SCRATCH_IMAGE_BYTES: u64 = 512 << 20;
pub const GUEST_MIN_MEMORY_MIB: u32 = 128;
pub const MAX_VCPUS: u32 = 32;
pub const TMPFS_SIZE: &str = "size=64m";
/// Where the image installs the agent; the kernel starts it as PID 1 (`init=`).
pub const INIT_PATH: &str = "/sbin/agentos-guest";
/// The guest kernel command line; Firecracker appends `root=/dev/vda ro` and the
/// `virtio_mmio.device=` entries. It carries nothing secret. Here (not in the engine) so the
/// guest can check it names `INIT_PATH`.
pub const BOOT_ARGS: &str =
    "console=ttyS0 reboot=k panic=1 pci=off nomodule quiet loglevel=4 init=/sbin/agentos-guest";
/// Without a bound `Hello` this long after boot, the guest shuts itself down.
pub const HELLO_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Job,
    Inspect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchStateKind {
    NotApplied,
    Applied,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    Hello {
        protocol: u32,
        attempt_token: String,
        task_id: String,
        effect_id: String,
        attempt_id: String,
        lease_generation: u64,
        mode: Mode,
    },
    Ready {
        protocol: u32,
        agent_version: String,
        mode: Mode,
        vcpus: u32,
        memory_mib: u32,
    },
    ReadSnapshot {
        file_count: u64,
        total_bytes: u64,
    },
    File {
        path: String,
        len: u64,
    },
    EndFiles,
    SnapshotDone {
        files: Vec<String>,
        workspace_digest: Digest,
    },
    ApplyPatch {
        expected_base: Digest,
        editable_paths: Vec<String>,
    },
    PatchApplied {
        paths: Vec<String>,
        workspace_digest: Digest,
    },
    RunVerification {
        profile_digest: Option<Digest>,
        timeout_secs: u64,
        file_count: u64,
        total_bytes: u64,
    },
    Verified {
        profile_id: String,
        command: Vec<String>,
        profile_digest: Digest,
        workspace_digest: Digest,
        exit_code: Option<i32>,
        stdout_b64: String,
        stdout_truncated: bool,
        stderr_b64: String,
        stderr_truncated: bool,
    },
    Digest,
    DigestIs {
        workspace_digest: Digest,
    },
    PatchState {
        expected_base: Digest,
    },
    PatchStateIs {
        state: PatchStateKind,
        paths: Vec<String>,
        workspace_digest: Option<Digest>,
        reason: Option<String>,
    },
    /// Host -> guest, Job mode only. Starts the agent CLI in a scratch copy of the workspace.
    /// No raw frame follows.
    RunAgent {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        timeout_secs: u64,
        expected_base: Digest,
    },
    /// Guest -> host. The agent CLI called the model through the guest's loopback proxy. One
    /// raw frame (the request body) follows.
    ModelRequest {
        id: u64,
    },
    /// Host -> guest. The answer to `ModelRequest { id }`. One raw frame (the response body)
    /// follows.
    ModelReply {
        id: u64,
        status: u16,
    },
    /// Guest -> host. The agent CLI exited, or was killed at its deadline. One raw frame (the
    /// patch, empty if none) follows.
    AgentDone {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        workspace_digest: Digest,
    },
    Refused {
        reason: String,
    },
    Shutdown,
    Bye,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 0,
    Raw = 1,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FrameKind::Json => "json",
            FrameKind::Raw => "raw",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Json(Message),
    Raw(Vec<u8>),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame too large: {kind} {len} > {limit}")]
    TooLarge {
        kind: FrameKind,
        len: usize,
        limit: usize,
    },
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("invalid json frame: {0}")]
    Json(String),
    #[error("frame io: {0}")]
    Io(#[from] io::Error),
}

/// Writes `u32 BE body length`, `u8 kind`, then the body.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (kind, json);
    let body: &[u8] = match frame {
        Frame::Json(m) => {
            kind = FrameKind::Json;
            json = serde_json::to_vec(m).map_err(io::Error::other)?;
            &json
        }
        Frame::Raw(b) => {
            kind = FrameKind::Raw;
            b
        }
    };
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame body exceeds u32"))?;
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&len.to_be_bytes());
    header[4] = kind as u8;
    w.write_all(&header)?;
    w.write_all(body)?;
    w.flush()
}

/// Reads one frame. The 5-byte header is checked against the limits before any body allocation.
pub fn read_frame(r: &mut impl Read, raw_limit: usize) -> Result<Frame, FrameError> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let (kind, limit) = match header[4] {
        0 => (FrameKind::Json, JSON_FRAME_LIMIT),
        1 => (FrameKind::Raw, raw_limit),
        k => return Err(FrameError::UnknownKind(k)),
    };
    if len > limit {
        return Err(FrameError::TooLarge { kind, len, limit });
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    match kind {
        FrameKind::Json => serde_json::from_slice(&body)
            .map(Frame::Json)
            .map_err(|e| FrameError::Json(e.to_string())),
        FrameKind::Raw => Ok(Frame::Raw(body)),
    }
}

/// Number of raw frames that carry a file of `len` bytes (every frame full except the last).
pub fn raw_frames_for(len: u64) -> u64 {
    len.div_ceil(RAW_FRAME_LIMIT as u64)
}

pub fn mint_attempt_token() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_attempt_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn d(n: u8) -> Digest {
        Digest::of(&[n])
    }

    fn all_messages() -> Vec<(&'static str, Message)> {
        vec![
            (
                "Hello",
                Message::Hello {
                    protocol: GUEST_PROTOCOL,
                    attempt_token: "a".repeat(32),
                    task_id: "t".into(),
                    effect_id: "e".into(),
                    attempt_id: "at".into(),
                    lease_generation: 7,
                    mode: Mode::Job,
                },
            ),
            (
                "Ready",
                Message::Ready {
                    protocol: GUEST_PROTOCOL,
                    agent_version: "0.1.0".into(),
                    mode: Mode::Inspect,
                    vcpus: 2,
                    memory_mib: 256,
                },
            ),
            (
                "ReadSnapshot",
                Message::ReadSnapshot {
                    file_count: 3,
                    total_bytes: 99,
                },
            ),
            (
                "File",
                Message::File {
                    path: "src/a.py".into(),
                    len: 5,
                },
            ),
            ("EndFiles", Message::EndFiles),
            (
                "SnapshotDone",
                Message::SnapshotDone {
                    files: vec!["a".into(), "b".into()],
                    workspace_digest: d(1),
                },
            ),
            (
                "ApplyPatch",
                Message::ApplyPatch {
                    expected_base: d(2),
                    editable_paths: vec!["src/**".into()],
                },
            ),
            (
                "PatchApplied",
                Message::PatchApplied {
                    paths: vec!["src/a.py".into()],
                    workspace_digest: d(3),
                },
            ),
            (
                "RunVerification",
                Message::RunVerification {
                    profile_digest: Some(d(4)),
                    timeout_secs: 60,
                    file_count: 1,
                    total_bytes: 10,
                },
            ),
            (
                "Verified",
                Message::Verified {
                    profile_id: "p".into(),
                    command: vec!["python3".into(), "-m".into()],
                    profile_digest: d(5),
                    workspace_digest: d(6),
                    exit_code: Some(0),
                    stdout_b64: b64(b"out"),
                    stdout_truncated: false,
                    stderr_b64: b64(b""),
                    stderr_truncated: true,
                },
            ),
            ("Digest", Message::Digest),
            (
                "DigestIs",
                Message::DigestIs {
                    workspace_digest: d(7),
                },
            ),
            (
                "PatchState",
                Message::PatchState {
                    expected_base: d(8),
                },
            ),
            (
                "PatchStateIs",
                Message::PatchStateIs {
                    state: PatchStateKind::Unknown,
                    paths: vec![],
                    workspace_digest: None,
                    reason: Some("why".into()),
                },
            ),
            (
                "Refused",
                Message::Refused {
                    reason: "no".into(),
                },
            ),
            ("Shutdown", Message::Shutdown),
            ("Bye", Message::Bye),
            (
                "RunAgent",
                Message::RunAgent {
                    argv: vec!["claude".into(), "-p".into()],
                    env: vec![("HOME".into(), "/scratch".into())],
                    timeout_secs: 300,
                    expected_base: d(9),
                },
            ),
            ("ModelRequest", Message::ModelRequest { id: 1 }),
            ("ModelReply", Message::ModelReply { id: 1, status: 200 }),
            (
                "AgentDone",
                Message::AgentDone {
                    exit_code: None,
                    signal: Some(9),
                    timed_out: true,
                    workspace_digest: d(10),
                },
            ),
        ]
    }

    #[test]
    fn every_message_round_trips_through_json_with_its_type_tag() {
        let all = all_messages();
        assert_eq!(all.len(), 21);
        for (tag, m) in all {
            let json = serde_json::to_string(&m).unwrap();
            assert!(json.contains(&format!("\"type\":\"{tag}\"")), "{json}");
            let back: Message = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
        assert!(serde_json::to_string(&Mode::Job).unwrap() == "\"job\"");
        assert!(serde_json::to_string(&PatchStateKind::NotApplied).unwrap() == "\"not_applied\"");
    }

    fn json_body_of_len(n: usize) -> Message {
        // Refused{reason} serializes to a fixed overhead plus the reason's bytes.
        let overhead = serde_json::to_vec(&Message::Refused {
            reason: String::new(),
        })
        .unwrap()
        .len();
        Message::Refused {
            reason: "x".repeat(n - overhead),
        }
    }

    #[test]
    fn frame_round_trips_json_and_raw_at_the_limits() {
        let m = json_body_of_len(JSON_FRAME_LIMIT);
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Json(m.clone())).unwrap();
        assert_eq!(buf.len(), 5 + JSON_FRAME_LIMIT);
        assert_eq!(
            read_frame(&mut Cursor::new(&buf), RAW_FRAME_LIMIT).unwrap(),
            Frame::Json(m)
        );

        let raw = vec![0xabu8; RAW_FRAME_LIMIT];
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Raw(raw.clone())).unwrap();
        assert_eq!(&buf[..5], &[0x01, 0x00, 0x00, 0x00, 1]);
        assert_eq!(
            read_frame(&mut Cursor::new(&buf), RAW_FRAME_LIMIT).unwrap(),
            Frame::Raw(raw)
        );
    }

    #[test]
    fn decode_rejects_a_length_over_the_limit_without_allocating() {
        let mut hdr = u32::MAX.to_be_bytes().to_vec();
        hdr.push(FrameKind::Raw as u8);
        let err = read_frame(&mut Cursor::new(hdr), RAW_FRAME_LIMIT).unwrap_err();
        assert!(
            matches!(err, FrameError::TooLarge { kind: FrameKind::Raw, len, limit } if len == u32::MAX as usize && limit == RAW_FRAME_LIMIT),
            "{err:?}"
        );

        let mut hdr = ((JSON_FRAME_LIMIT + 1) as u32).to_be_bytes().to_vec();
        hdr.push(FrameKind::Json as u8);
        let err = read_frame(&mut Cursor::new(hdr), RAW_FRAME_LIMIT).unwrap_err();
        assert!(
            matches!(
                err,
                FrameError::TooLarge {
                    kind: FrameKind::Json,
                    ..
                }
            ),
            "{err:?}"
        );

        let mut hdr = (17u32 << 20).to_be_bytes().to_vec();
        hdr.push(1);
        let err = read_frame(&mut Cursor::new(hdr), RAW_FRAME_LIMIT).unwrap_err();
        assert_eq!(err.to_string(), "frame too large: raw 17825792 > 16777216");

        // A smaller caller-chosen raw limit is honoured.
        let mut hdr = 11u32.to_be_bytes().to_vec();
        hdr.push(1);
        assert!(matches!(
            read_frame(&mut Cursor::new(hdr), 10).unwrap_err(),
            FrameError::TooLarge { limit: 10, .. }
        ));
    }

    #[test]
    fn decode_rejects_an_unknown_kind_and_invalid_json() {
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.extend([9, 0]);
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), RAW_FRAME_LIMIT).unwrap_err(),
            FrameError::UnknownKind(9)
        ));
        let mut buf = 3u32.to_be_bytes().to_vec();
        buf.push(0);
        buf.extend(b"{x}");
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), RAW_FRAME_LIMIT).unwrap_err(),
            FrameError::Json(_)
        ));
        // Valid JSON of an unknown message type is also a Json error.
        let body = br#"{"type":"Nope"}"#;
        let mut buf = (body.len() as u32).to_be_bytes().to_vec();
        buf.push(0);
        buf.extend(body);
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), RAW_FRAME_LIMIT).unwrap_err(),
            FrameError::Json(_)
        ));
    }

    #[test]
    fn truncated_body_is_an_io_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Raw(vec![1; 10])).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), RAW_FRAME_LIMIT).unwrap_err(),
            FrameError::Io(_)
        ));
        assert!(matches!(
            read_frame(&mut Cursor::new(vec![0u8, 0]), RAW_FRAME_LIMIT).unwrap_err(),
            FrameError::Io(_)
        ));
    }

    #[test]
    fn raw_frames_for_matches_the_split_rule() {
        let l = RAW_FRAME_LIMIT as u64;
        assert_eq!(raw_frames_for(0), 0);
        assert_eq!(raw_frames_for(1), 1);
        assert_eq!(raw_frames_for(l), 1);
        assert_eq!(raw_frames_for(l + 1), 2);
        assert_eq!(raw_frames_for(FILE_LIMIT), 4);
    }

    #[test]
    fn attempt_tokens_are_32_lowercase_hex_and_unique() {
        let a = mint_attempt_token();
        let b = mint_attempt_token();
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.len(), 32);
            assert!(is_attempt_token(t), "{t}");
        }
    }

    #[test]
    fn is_attempt_token_rejects_31_33_uppercase_and_non_hex() {
        let ok = "0123456789abcdef0123456789abcdef";
        assert!(is_attempt_token(ok));
        assert!(!is_attempt_token(&ok[..31]));
        assert!(!is_attempt_token(&format!("{ok}0")));
        assert!(!is_attempt_token(&ok.to_uppercase()));
        assert!(!is_attempt_token(&format!("{}g", &ok[..31])));
        assert!(!is_attempt_token(""));
    }

    #[test]
    fn b64_round_trips_and_rejects_garbage() {
        for bytes in [&b""[..], b"a", b"ab", b"abc", &[0, 255, 254, 1]] {
            assert_eq!(unb64(&b64(bytes)).unwrap(), bytes);
        }
        assert_eq!(b64(b"a"), "YQ==");
        assert!(unb64("not base64!").is_err());
        assert!(unb64("YQ").is_err(), "unpadded input is rejected");
    }
}
