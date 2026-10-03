//! The fake guest end to end: `agentos-guest --fake UDS ROOT` as the host will reach it.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{read_frame, write_frame, Frame, FrameError, Message, Mode, RAW_FRAME_LIMIT};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210";

struct FakeGuest {
    child: Child,
    uds: PathBuf,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for FakeGuest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_with(env: &[(&str, &str)], prepare: impl FnOnce(&Path)) -> FakeGuest {
    let dir = tempfile::tempdir().unwrap();
    let (uds, root) = (dir.path().join("v.sock"), dir.path().join("root"));
    prepare(&root);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentos-guest"));
    cmd.arg("--fake").arg(&uds).arg(&root).stdin(Stdio::null());
    // Only the variables a test passes reach the guest, whatever the outer environment holds.
    cmd.env_remove("AGENTOS_TEST_WORKERS")
        .env_remove("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN")
        .env_remove("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS");
    for (k, v) in env {
        cmd.env(k, v);
    }
    FakeGuest { child: cmd.spawn().unwrap(), uds, root, _dir: dir }
}

fn spawn() -> FakeGuest {
    spawn_with(&[("AGENTOS_TEST_WORKERS", "1")], |_| {})
}

fn wait_for(path: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

impl FakeGuest {
    /// Connects and completes the `CONNECT 5200` / `OK 5200` handshake.
    fn connect(&self) -> UnixStream {
        assert!(wait_for(&self.uds, Duration::from_secs(5)), "the fake guest never listened");
        let mut s = UnixStream::connect(&self.uds).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(b"CONNECT 5200\n").unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while byte[0] != b'\n' {
            s.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        assert_eq!(line, b"OK 5200\n");
        s
    }

    fn hello(&self, token: &str, mode: Mode) -> UnixStream {
        let mut s = self.connect();
        send(&mut s, hello_msg(1, token, mode));
        let Message::Ready { mode: got, .. } = recv(&mut s) else { panic!("no Ready") };
        assert_eq!(got, mode);
        s
    }
}

fn hello_msg(protocol: u32, token: &str, mode: Mode) -> Message {
    Message::Hello {
        protocol,
        attempt_token: token.into(),
        task_id: "t".into(),
        effect_id: "e".into(),
        attempt_id: "a".into(),
        lease_generation: 1,
        mode,
    }
}

fn send(s: &mut UnixStream, m: Message) {
    write_frame(s, &Frame::Json(m)).unwrap();
}

fn recv(s: &mut UnixStream) -> Message {
    match read_frame(s, RAW_FRAME_LIMIT).unwrap() {
        Frame::Json(m) => m,
        Frame::Raw(_) => panic!("unexpected raw frame"),
    }
}

/// The peer closed the connection without sending another frame.
fn assert_closed(s: &mut UnixStream) {
    match read_frame(s, RAW_FRAME_LIMIT) {
        Err(FrameError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{e}"),
        other => panic!("expected the connection to be closed, got {other:?}"),
    }
}

fn empty_snapshot(s: &mut UnixStream) {
    send(s, Message::ReadSnapshot { file_count: 0, total_bytes: 0 });
    send(s, Message::EndFiles);
    let Message::SnapshotDone { files, .. } = recv(s) else { panic!("no SnapshotDone") };
    assert!(files.is_empty());
}

#[test]
fn fake_guest_answers_connect_with_ok_and_hello_with_ready() {
    let g = spawn();
    let mut s = g.connect();
    send(&mut s, hello_msg(1, TOKEN, Mode::Job));
    let Message::Ready { protocol, agent_version, mode, vcpus, memory_mib } = recv(&mut s) else { panic!("no Ready") };
    assert_eq!((protocol, agent_version.as_str(), mode), (1, "0.1.0", Mode::Job));
    assert!(vcpus >= 1 && memory_mib > 0, "{vcpus} {memory_mib}");
}

#[test]
fn a_bad_connect_line_is_closed_and_the_guest_keeps_listening() {
    let g = spawn();
    assert!(wait_for(&g.uds, Duration::from_secs(5)));
    let mut s = UnixStream::connect(&g.uds).unwrap();
    s.write_all(b"CONNECT 5201\n").unwrap();
    let mut buf = Vec::new();
    assert_eq!(s.read_to_end(&mut buf).unwrap(), 0);
    let _ok = g.hello(TOKEN, Mode::Job);
}

#[test]
fn a_hello_with_protocol_2_is_refused_and_the_connection_closed() {
    let g = spawn();
    let mut s = g.connect();
    send(&mut s, hello_msg(2, TOKEN, Mode::Job));
    let Message::Refused { reason } = recv(&mut s) else { panic!("no Refused") };
    assert!(reason.contains("protocol 2"), "{reason}");
    assert_closed(&mut s);
}

#[test]
fn a_second_connection_with_another_token_is_closed_without_a_reply() {
    let g = spawn();
    let mut first = g.hello(TOKEN, Mode::Job);
    let mut second = g.connect();
    send(&mut second, hello_msg(1, OTHER, Mode::Job));
    assert_closed(&mut second);
    // The bound session is unaffected.
    empty_snapshot(&mut first);
}

#[test]
fn a_second_connection_with_the_same_token_is_served() {
    let g = spawn();
    let _first = g.hello(TOKEN, Mode::Job);
    let mut second = g.hello(TOKEN, Mode::Job);
    empty_snapshot(&mut second);
}

#[test]
fn a_second_connection_with_the_same_token_but_another_mode_is_closed_without_a_reply() {
    let g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1")], |root| {
        fs::create_dir_all(root.join("workspace")).unwrap();
    });
    let mut first = g.hello(TOKEN, Mode::Job);
    // The mode is bound with the token for the whole process: an inspect Hello mid-job
    // must never reach the inspect path (the VM would remount the workspace read-only).
    let mut second = g.connect();
    send(&mut second, hello_msg(1, TOKEN, Mode::Inspect));
    assert_closed(&mut second);
    empty_snapshot(&mut first);

    // And the other way round: an inspection never becomes a job.
    let g = spawn();
    let mut first = g.hello(TOKEN, Mode::Inspect);
    let mut second = g.connect();
    send(&mut second, hello_msg(1, TOKEN, Mode::Job));
    assert_closed(&mut second);
    send(&mut first, Message::Digest);
    assert!(matches!(recv(&mut first), Message::Refused { .. } | Message::DigestIs { .. }));
}

#[test]
fn the_watchdog_ends_a_guest_that_never_gets_a_hello() {
    let mut g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1"), ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300")], |_| {});
    // A connection that never says Hello does not count.
    let _silent = g.connect();
    let status = wait_exit(&mut g.child, Duration::from_secs(5)).expect("the watchdog never fired");
    assert_eq!(status.code(), Some(0));

    // A bound Hello disarms it.
    let mut g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1"), ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300")], |_| {});
    let mut s = g.hello(TOKEN, Mode::Job);
    thread::sleep(Duration::from_millis(800));
    assert!(g.child.try_wait().unwrap().is_none(), "the watchdog fired after a bound Hello");
    empty_snapshot(&mut s);

    // A rejected Hello (another protocol) does not disarm it either.
    let mut g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1"), ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300")], |_| {});
    let mut s = g.connect();
    send(&mut s, hello_msg(2, TOKEN, Mode::Job));
    let status = wait_exit(&mut g.child, Duration::from_secs(5)).expect("the watchdog never fired");
    assert_eq!(status.code(), Some(0));

    // The shortening hook is honoured only with AGENTOS_TEST_WORKERS=1.
    let mut g = spawn_with(&[("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300")], |_| {});
    assert!(wait_for(&g.uds, Duration::from_secs(5)));
    thread::sleep(Duration::from_millis(800));
    assert!(g.child.try_wait().unwrap().is_none(), "the hook was honoured without AGENTOS_TEST_WORKERS=1");
}

#[test]
fn inspect_mode_refuses_job_requests_and_job_mode_refuses_inspect_requests() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Inspect);
    send(&mut s, Message::ReadSnapshot { file_count: 0, total_bytes: 0 });
    let Message::Refused { reason } = recv(&mut s) else { panic!("no Refused") };
    assert_eq!(reason, "unexpected request ReadSnapshot in inspect mode");
    assert_closed(&mut s);

    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    send(&mut s, Message::Digest);
    let Message::Refused { reason } = recv(&mut s) else { panic!("no Refused") };
    assert_eq!(reason, "unexpected request Digest in job mode");
    assert_closed(&mut s);

    // Inspect mode answers its own requests over the persisted workspace.
    let g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1")], |root| {
        fs::create_dir_all(root.join("workspace/src")).unwrap();
        fs::write(root.join("workspace/src/a.py"), "a = 1\n").unwrap();
    });
    let mut s = g.hello(TOKEN, Mode::Inspect);
    send(&mut s, Message::Digest);
    let Message::DigestIs { workspace_digest } = recv(&mut s) else { panic!("no DigestIs") };
    assert_eq!(workspace_digest, agentos_core::workspace::workspace_digest(&g.root.join("workspace")).unwrap());
}

#[test]
fn shutdown_gets_bye_and_the_process_exits_0_within_1s() {
    let mut g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    send(&mut s, Message::Shutdown);
    assert_eq!(recv(&mut s), Message::Bye);
    let status = wait_exit(&mut g.child, Duration::from_secs(1)).expect("still running 1 s after Bye");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn eof_makes_the_fake_guest_exit_within_500ms() {
    let mut g = spawn();
    let s = g.hello(TOKEN, Mode::Job);
    drop(s);
    let status = wait_exit(&mut g.child, Duration::from_millis(500)).expect("still running 500 ms after EOF");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn never_listen_hook_leaves_no_socket() {
    let mut g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1"), ("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN", "1")], |_| {});
    assert!(!wait_for(&g.uds, Duration::from_secs(1)), "the hook must keep the socket away");
    assert!(g.child.try_wait().unwrap().is_none(), "the hooked guest sleeps rather than exiting");

    // Without AGENTOS_TEST_WORKERS=1 the hook is ignored.
    let g = spawn_with(&[("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN", "1")], |_| {});
    assert!(wait_for(&g.uds, Duration::from_secs(5)));
}

#[test]
fn fake_start_recreates_an_empty_scratch_dir() {
    let g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1")], |root| {
        fs::create_dir_all(root.join("scratch/old")).unwrap();
        fs::write(root.join("scratch/old/leftover"), "x").unwrap();
    });
    assert!(wait_for(&g.uds, Duration::from_secs(5)));
    let scratch = g.root.join("scratch");
    assert!(scratch.is_dir());
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
}

#[test]
fn no_arguments_prints_the_usage_and_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentos-guest")).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage: agentos-guest --fake UDS ROOT"), "{stderr}");
    // The exec-check line is the trampoline's own usage, with every required option.
    let exec_check = agentos_guest::trampoline::USAGE.trim_start_matches("usage: ");
    assert!(stderr.contains(exec_check), "{stderr}");
    assert!(exec_check.contains("--uid U --gid G"), "{exec_check}");
}
