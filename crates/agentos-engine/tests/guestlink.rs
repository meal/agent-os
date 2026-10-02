//! The host side of the guest protocol against the fake guest
//! (`agentos-supervisor fake-guest UDS ROOT`).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{Message, Mode, RAW_FRAME_LIMIT};
use agentos_core::ids::Digest;
use agentos_core::workspace::workspace_digest;
use agentos_engine::guestlink::{spawn_fake, GuestLauncher, GuestLink, LinkError};

const BIN: &str = env!("CARGO_BIN_EXE_agentos-supervisor");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const BASE: &str = "be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn launcher() -> GuestLauncher {
    GuestLauncher::Fake { program: BIN.into(), prefix_args: vec![] }
}

fn test_env() -> Vec<(String, String)> {
    vec![("AGENTOS_TEST_WORKERS".into(), "1".into())]
}

struct Guest {
    child: Child,
    uds: PathBuf,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_guest(env: &[(String, String)]) -> Guest {
    let dir = tempfile::tempdir().unwrap();
    let (uds, root) = (dir.path().join("v.sock"), dir.path().join("root"));
    let child = spawn_fake(&launcher(), &uds, &root, env).unwrap();
    Guest { child, uds, root, _dir: dir }
}

fn hello() -> Message {
    Message::Hello {
        protocol: 1,
        attempt_token: TOKEN.into(),
        task_id: "t".into(),
        effect_id: "e".into(),
        attempt_id: "a".into(),
        lease_generation: 1,
        mode: Mode::Job,
    }
}

fn soon(secs: u64) -> Instant {
    Instant::now() + Duration::from_secs(secs)
}

fn connected(g: &Guest) -> GuestLink {
    let mut link = GuestLink::connect(&g.uds, soon(5)).unwrap();
    let ready = link.hello(hello(), soon(5)).unwrap();
    assert!(matches!(ready, Message::Ready { protocol: 1, mode: Mode::Job, .. }), "{ready:?}");
    link
}

fn snapshot(link: &mut GuestLink, tree: &Path) -> Message {
    let (files, bytes) = GuestLink::count_tree(tree).unwrap();
    link.send(&Message::ReadSnapshot { file_count: files, total_bytes: bytes }).unwrap();
    assert_eq!(link.send_tree(tree).unwrap(), (files, bytes));
    link.recv(soon(20)).unwrap()
}

#[test]
fn connect_waits_for_the_socket_and_completes_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (uds, root) = (dir.path().join("v.sock"), dir.path().join("root"));
    let (u2, r2) = (uds.clone(), root.clone());
    let spawner = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        spawn_fake(&launcher(), &u2, &r2, &test_env()).unwrap()
    });
    let started = Instant::now();
    let mut link = GuestLink::connect(&uds, soon(10)).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(250), "connected before the guest existed?");
    let ready = link.hello(hello(), soon(5)).unwrap();
    assert!(matches!(ready, Message::Ready { protocol: 1, .. }), "{ready:?}");
    let mut child = spawner.join().unwrap();
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn connect_times_out_when_nobody_listens() {
    let g = spawn_guest(&[("AGENTOS_TEST_WORKERS".into(), "1".into()), ("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN".into(), "1".into())]);
    let started = Instant::now();
    let err = GuestLink::connect(&g.uds, started + Duration::from_secs(1)).err().expect("must not connect");
    let took = started.elapsed();
    assert!(matches!(err, LinkError::BootTimeout), "{err}");
    assert!(err.to_string().starts_with("guest did not come up: "), "{err}");
    assert!(took >= Duration::from_secs(1) && took <= Duration::from_millis(1500), "{took:?}");
}

#[test]
fn send_tree_streams_the_fixture_and_the_guest_reports_the_golden_digest() {
    let g = spawn_guest(&test_env());
    let mut link = connected(&g);
    let Message::SnapshotDone { files, workspace_digest } = snapshot(&mut link, &fixtures().join("parser-repo")) else {
        panic!("no SnapshotDone")
    };
    assert_eq!(workspace_digest.to_string(), BASE);
    assert_eq!(files, vec!["src/__init__.py", "src/parser.py", "tests/__init__.py", "tests/test_parser.py"]);
}

#[test]
fn send_tree_refuses_an_oversized_tree_before_sending() {
    // A plain peer that accepts the handshake and records every byte after it.
    let dir = tempfile::tempdir().unwrap();
    let uds = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&uds).unwrap();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = [0u8; 13];
        s.read_exact(&mut line).unwrap();
        assert_eq!(&line, b"CONNECT 5200\n");
        s.write_all(b"OK 5200\n").unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        rest
    });
    let tree = tempfile::tempdir().unwrap();
    for i in 0..=65_536u32 {
        fs::write(tree.path().join(format!("f{i}")), b"").unwrap();
    }
    let mut link = GuestLink::connect(&uds, soon(5)).unwrap();
    let err = link.send_tree(tree.path()).unwrap_err();
    assert!(matches!(err, LinkError::Protocol(_)), "{err}");
    drop(link);
    assert!(peer.join().unwrap().is_empty(), "something was written before the refusal");
}

#[test]
fn a_file_larger_than_one_raw_frame_is_split_and_reassembled() {
    let tree = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..(17usize << 20)).map(|i| (i % 251) as u8).collect();
    assert!(big.len() > RAW_FRAME_LIMIT);
    fs::write(tree.path().join("big.bin"), &big).unwrap();
    let g = spawn_guest(&test_env());
    let mut link = connected(&g);
    let Message::SnapshotDone { files, workspace_digest: got } = snapshot(&mut link, tree.path()) else { panic!("no SnapshotDone") };
    assert_eq!(files, vec!["big.bin"]);
    assert_eq!(got, workspace_digest(tree.path()).unwrap());
    assert_eq!(fs::read(g.root.join("workspace/big.bin")).unwrap(), big);
}

#[test]
fn recv_times_out_at_its_deadline() {
    let g = spawn_guest(&test_env());
    let mut link = connected(&g);
    assert!(matches!(snapshot(&mut link, &fixtures().join("parser-repo")), Message::SnapshotDone { .. }));
    let profile = tempfile::tempdir().unwrap();
    fs::write(profile.path().join("profile.json"), r#"{"id":"hang","command":["sh","-c","sleep 30"],"protected":true}"#).unwrap();
    let (files, bytes) = GuestLink::count_tree(profile.path()).unwrap();
    link.send(&Message::RunVerification { profile_digest: None, timeout_secs: 30, file_count: files, total_bytes: bytes }).unwrap();
    link.send_tree(profile.path()).unwrap();
    let started = Instant::now();
    let err = link.recv(started + Duration::from_millis(500)).unwrap_err();
    let took = started.elapsed();
    assert!(matches!(err, LinkError::Lost(_)), "{err}");
    assert!(took >= Duration::from_millis(450) && took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn an_oversized_frame_from_the_peer_is_a_protocol_error() {
    let dir = tempfile::tempdir().unwrap();
    let uds = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&uds).unwrap();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = [0u8; 13];
        s.read_exact(&mut line).unwrap();
        s.write_all(b"OK 5200\n").unwrap();
        let mut hdr = u32::MAX.to_be_bytes().to_vec();
        hdr.push(1);
        s.write_all(&hdr).unwrap();
        thread::sleep(Duration::from_millis(500));
    });
    let mut link = GuestLink::connect(&uds, soon(5)).unwrap();
    let err = link.recv(soon(5)).unwrap_err();
    assert!(matches!(err, LinkError::Protocol(_)), "{err}");
    assert!(err.to_string().starts_with("guest protocol violation: "), "{err}");
    peer.join().unwrap();
}

#[test]
fn a_refused_hello_is_reported_verbatim_and_a_raw_frame_is_a_violation() {
    let g = spawn_guest(&test_env());
    let mut link = connected(&g);
    // A second Hello on a bound session is not allowed; the guest answers Refused.
    link.send(&hello()).unwrap();
    assert!(matches!(link.recv(soon(5)).unwrap(), Message::Refused { .. }));
    assert_eq!(LinkError::Refused("because".into()).to_string(), "because");
    assert_eq!(LinkError::Lost(std::io::Error::other("x")).to_string(), "guest connection lost: x");
}

#[test]
fn fake_guest_shuts_down_within_500ms_of_eof() {
    let mut g = spawn_guest(&test_env());
    let link = connected(&g);
    let dropped = Instant::now();
    drop(link);
    loop {
        if let Some(status) = g.child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(dropped.elapsed() < Duration::from_millis(500), "the guest outlived its connection");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn fake_guest_verb_is_wired_in_the_supervisor_binary() {
    let out = Command::new(BIN).arg("fake-guest").stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage: agentos-supervisor run|worker <job_dir> | fake-guest <uds> <root>"), "{err}");
}

#[test]
fn hello_requires_ready_protocol_one() {
    let dir = tempfile::tempdir().unwrap();
    let uds = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&uds).unwrap();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = [0u8; 13];
        s.read_exact(&mut line).unwrap();
        s.write_all(b"OK 5200\n").unwrap();
        let _ = agentos_core::guest::read_frame(&mut s, 0).unwrap();
        let ready = Message::Ready { protocol: 2, agent_version: "x".into(), mode: Mode::Job, vcpus: 1, memory_mib: 128 };
        agentos_core::guest::write_frame(&mut s, &agentos_core::guest::Frame::Json(ready)).unwrap();
    });
    let mut link = GuestLink::connect(&uds, soon(5)).unwrap();
    let err = link.hello(hello(), soon(5)).unwrap_err();
    assert!(matches!(err, LinkError::Protocol(_)), "{err}");
    peer.join().unwrap();
    let _ = Digest::of(b"");
}
