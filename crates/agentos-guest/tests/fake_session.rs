//! The fake guest end to end: `agentos-guest --fake UDS ROOT` as the host will reach it.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{
    Frame, FrameError, Message, Mode, RAW_FRAME_LIMIT, read_frame, write_frame,
};
use agentos_core::ids::Digest;

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
    FakeGuest {
        child: cmd.spawn().unwrap(),
        uds,
        root,
        _dir: dir,
    }
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
        assert!(
            wait_for(&self.uds, Duration::from_secs(5)),
            "the fake guest never listened"
        );
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
        send(&mut s, hello_msg(2, token, mode));
        let Message::Ready { mode: got, .. } = recv(&mut s) else {
            panic!("no Ready")
        };
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
    send(
        s,
        Message::ReadSnapshot {
            file_count: 0,
            total_bytes: 0,
        },
    );
    send(s, Message::EndFiles);
    let Message::SnapshotDone { files, .. } = recv(s) else {
        panic!("no SnapshotDone")
    };
    assert!(files.is_empty());
}

#[test]
fn fake_guest_answers_connect_with_ok_and_hello_with_ready() {
    let g = spawn();
    let mut s = g.connect();
    send(&mut s, hello_msg(2, TOKEN, Mode::Job));
    let Message::Ready {
        protocol,
        agent_version,
        mode,
        vcpus,
        memory_mib,
    } = recv(&mut s)
    else {
        panic!("no Ready")
    };
    assert_eq!(
        (protocol, agent_version.as_str(), mode),
        (2, "0.2.0", Mode::Job)
    );
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
fn a_hello_with_protocol_1_is_refused_and_the_connection_closed() {
    let g = spawn();
    let mut s = g.connect();
    send(&mut s, hello_msg(1, TOKEN, Mode::Job));
    let Message::Refused { reason } = recv(&mut s) else {
        panic!("no Refused")
    };
    assert_eq!(
        reason,
        "unsupported protocol 1, this agent speaks protocol 2"
    );
    assert_closed(&mut s);
}

fn run_agent_msg() -> Message {
    Message::RunAgent {
        argv: vec!["/bin/true".into()],
        env: vec![],
        timeout_secs: 5,
        expected_base: agentos_core::ids::Digest::of(b"base"),
    }
}

#[test]
fn run_agent_in_inspect_mode_is_refused_and_the_session_lost() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Inspect);
    send(&mut s, run_agent_msg());
    let Message::Refused { reason } = recv(&mut s) else {
        panic!("no Refused")
    };
    assert_eq!(reason, "unexpected request RunAgent in inspect mode");
    assert_closed(&mut s);
}

#[test]
fn run_agent_before_any_snapshot_is_refused_and_the_session_goes_on() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    send(&mut s, run_agent_msg());
    let Message::Refused { reason } = recv(&mut s) else {
        panic!("no Refused")
    };
    assert_eq!(reason, "workspace missing: no snapshot was read");
    empty_snapshot(&mut s);
}

#[test]
fn a_second_connection_with_another_token_is_closed_without_a_reply() {
    let g = spawn();
    let mut first = g.hello(TOKEN, Mode::Job);
    let mut second = g.connect();
    send(&mut second, hello_msg(2, OTHER, Mode::Job));
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
    send(&mut second, hello_msg(2, TOKEN, Mode::Inspect));
    assert_closed(&mut second);
    empty_snapshot(&mut first);

    // And the other way round: an inspection never becomes a job.
    let g = spawn();
    let mut first = g.hello(TOKEN, Mode::Inspect);
    let mut second = g.connect();
    send(&mut second, hello_msg(2, TOKEN, Mode::Job));
    assert_closed(&mut second);
    send(&mut first, Message::Digest);
    assert!(matches!(
        recv(&mut first),
        Message::Refused { .. } | Message::DigestIs { .. }
    ));
}

#[test]
fn the_watchdog_ends_a_guest_that_never_gets_a_hello() {
    let mut g = spawn_with(
        &[
            ("AGENTOS_TEST_WORKERS", "1"),
            ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300"),
        ],
        |_| {},
    );
    // A connection that never says Hello does not count.
    let _silent = g.connect();
    let status = wait_exit(&mut g.child, Duration::from_secs(5)).expect("the watchdog never fired");
    assert_eq!(status.code(), Some(0));

    // A bound Hello disarms it.
    let mut g = spawn_with(
        &[
            ("AGENTOS_TEST_WORKERS", "1"),
            ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300"),
        ],
        |_| {},
    );
    let mut s = g.hello(TOKEN, Mode::Job);
    thread::sleep(Duration::from_millis(800));
    assert!(
        g.child.try_wait().unwrap().is_none(),
        "the watchdog fired after a bound Hello"
    );
    empty_snapshot(&mut s);

    // A rejected Hello (another protocol) does not disarm it either.
    let mut g = spawn_with(
        &[
            ("AGENTOS_TEST_WORKERS", "1"),
            ("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300"),
        ],
        |_| {},
    );
    let mut s = g.connect();
    send(&mut s, hello_msg(1, TOKEN, Mode::Job));
    let status = wait_exit(&mut g.child, Duration::from_secs(5)).expect("the watchdog never fired");
    assert_eq!(status.code(), Some(0));

    // The shortening hook is honoured only with AGENTOS_TEST_WORKERS=1.
    let mut g = spawn_with(&[("AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS", "300")], |_| {});
    assert!(wait_for(&g.uds, Duration::from_secs(5)));
    thread::sleep(Duration::from_millis(800));
    assert!(
        g.child.try_wait().unwrap().is_none(),
        "the hook was honoured without AGENTOS_TEST_WORKERS=1"
    );
}

#[test]
fn inspect_mode_refuses_job_requests_and_job_mode_refuses_inspect_requests() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Inspect);
    send(
        &mut s,
        Message::ReadSnapshot {
            file_count: 0,
            total_bytes: 0,
        },
    );
    let Message::Refused { reason } = recv(&mut s) else {
        panic!("no Refused")
    };
    assert_eq!(reason, "unexpected request ReadSnapshot in inspect mode");
    assert_closed(&mut s);

    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    send(&mut s, Message::Digest);
    let Message::Refused { reason } = recv(&mut s) else {
        panic!("no Refused")
    };
    assert_eq!(reason, "unexpected request Digest in job mode");
    assert_closed(&mut s);

    // Inspect mode answers its own requests over the persisted workspace.
    let g = spawn_with(&[("AGENTOS_TEST_WORKERS", "1")], |root| {
        fs::create_dir_all(root.join("workspace/src")).unwrap();
        fs::write(root.join("workspace/src/a.py"), "a = 1\n").unwrap();
    });
    let mut s = g.hello(TOKEN, Mode::Inspect);
    send(&mut s, Message::Digest);
    let Message::DigestIs { workspace_digest } = recv(&mut s) else {
        panic!("no DigestIs")
    };
    assert_eq!(
        workspace_digest,
        agentos_core::workspace::workspace_digest(&g.root.join("workspace")).unwrap()
    );
}

#[test]
fn shutdown_gets_bye_and_the_process_exits_0_within_1s() {
    let mut g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    send(&mut s, Message::Shutdown);
    assert_eq!(recv(&mut s), Message::Bye);
    let status =
        wait_exit(&mut g.child, Duration::from_secs(1)).expect("still running 1 s after Bye");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn eof_makes_the_fake_guest_exit_within_500ms() {
    let mut g = spawn();
    let s = g.hello(TOKEN, Mode::Job);
    drop(s);
    let status = wait_exit(&mut g.child, Duration::from_millis(500))
        .expect("still running 500 ms after EOF");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn never_listen_hook_leaves_no_socket() {
    let mut g = spawn_with(
        &[
            ("AGENTOS_TEST_WORKERS", "1"),
            ("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN", "1"),
        ],
        |_| {},
    );
    assert!(
        !wait_for(&g.uds, Duration::from_secs(1)),
        "the hook must keep the socket away"
    );
    assert!(
        g.child.try_wait().unwrap().is_none(),
        "the hooked guest sleeps rather than exiting"
    );

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
    let out = Command::new(env!("CARGO_BIN_EXE_agentos-guest"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage: agentos-guest --fake UDS ROOT"),
        "{stderr}"
    );
    // The exec-check line is the trampoline's own usage, with every required option.
    let exec_check = agentos_guest::trampoline::USAGE.trim_start_matches("usage: ");
    assert!(stderr.contains(exec_check), "{stderr}");
    assert!(exec_check.contains("--uid U --gid G"), "{exec_check}");
}

// ---- RunAgent (Task 5): a scripted shell CLI runs in a scratch copy of the workspace ----

/// Snapshots `files` into the workspace and returns the digest the guest reports.
fn snapshot(s: &mut UnixStream, files: &[(&str, &[u8])]) -> Digest {
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    send(
        s,
        Message::ReadSnapshot {
            file_count: files.len() as u64,
            total_bytes: total,
        },
    );
    for (path, bytes) in files {
        send(
            s,
            Message::File {
                path: path.to_string(),
                len: bytes.len() as u64,
            },
        );
        if !bytes.is_empty() {
            write_frame(s, &Frame::Raw(bytes.to_vec())).unwrap();
        }
    }
    send(s, Message::EndFiles);
    let Message::SnapshotDone {
        workspace_digest, ..
    } = recv(s)
    else {
        panic!("no SnapshotDone")
    };
    workspace_digest
}

fn workspace_digest_of(dir: &Path) -> Digest {
    agentos_core::workspace::workspace_digest(dir).unwrap()
}

#[derive(Debug)]
enum Outcome {
    Done {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        workspace_digest: Digest,
        patch: Vec<u8>,
    },
    Refused(String),
}

/// Sends `RunAgent` and serves it as the host does: each `ModelRequest` gets `answer`'s
/// status and body back as a `ModelReply`. Returns the request bodies and the outcome.
fn run_agent(
    s: &mut UnixStream,
    argv: &[&str],
    env: &[(&str, &str)],
    timeout_secs: u64,
    expected_base: Digest,
    mut answer: impl FnMut(&[u8]) -> (u16, Vec<u8>),
) -> (Vec<Vec<u8>>, Outcome) {
    send(
        s,
        Message::RunAgent {
            argv: argv.iter().map(|a| a.to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            timeout_secs,
            expected_base,
        },
    );
    let mut requests = Vec::new();
    loop {
        match read_frame(s, RAW_FRAME_LIMIT).unwrap() {
            Frame::Json(Message::ModelRequest { id }) => {
                let Frame::Raw(body) = read_frame(s, RAW_FRAME_LIMIT).unwrap() else {
                    panic!("ModelRequest without its body")
                };
                let (status, reply) = answer(&body);
                requests.push(body);
                send(s, Message::ModelReply { id, status });
                write_frame(s, &Frame::Raw(reply)).unwrap();
            }
            Frame::Json(Message::AgentDone {
                exit_code,
                signal,
                timed_out,
                workspace_digest,
            }) => {
                let Frame::Raw(patch) = read_frame(s, RAW_FRAME_LIMIT).unwrap() else {
                    panic!("AgentDone without its patch")
                };
                return (
                    requests,
                    Outcome::Done {
                        exit_code,
                        signal,
                        timed_out,
                        workspace_digest,
                        patch,
                    },
                );
            }
            Frame::Json(Message::Refused { reason }) => {
                return (requests, Outcome::Refused(reason));
            }
            other => panic!("unexpected frame during RunAgent: {other:?}"),
        }
    }
}

/// Writes a shell script next to the fake guest's root (never inside the workspace).
fn write_cli(g: &FakeGuest, script: &str) -> String {
    let path = g.root.parent().unwrap().join("cli.sh");
    fs::write(&path, script).unwrap();
    path.display().to_string()
}

fn answer_json(n: &mut u32) -> impl FnMut(&[u8]) -> (u16, Vec<u8>) + '_ {
    move |_| {
        *n += 1;
        (200, format!("{{\"reply\":{n}}}").into_bytes())
    }
}

fn is_running(pid: &str) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the last `)` of the stat line; a zombie is no longer running.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

const CURL_POST: &str = "/usr/bin/curl -fsS -o \"$HOME/$OUT\" -X POST \"$ANTHROPIC_BASE_URL/v1/messages\" -H 'content-type: application/json' --data-binary @\"$HOME/$IN\"";

#[test]
fn run_agent_relays_each_model_call_and_returns_the_patch() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let script = format!(
        "set -eu\n\
         printf '%s' '{{\"model\":\"m\",\"max_tokens\":8,\"stream\":false,\"messages\":[]}}' > \"$HOME/req1.json\"\n\
         IN=req1.json\n\
         OUT=reply1.json\n\
         {CURL_POST}\n\
         printf 'edited\\n' > hello.txt\n\
         printf '%s' '{{\"model\":\"m\",\"max_tokens\":8,\"stream\":false,\"messages\":[{{\"role\":\"user\",\"content\":\"again\"}}]}}' > \"$HOME/req2.json\"\n\
         IN=req2.json\n\
         OUT=reply2.json\n\
         {CURL_POST}\n"
    );
    let cli = write_cli(&g, &script);
    let mut n = 0;
    let (requests, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    assert_eq!(requests.len(), 2, "one ModelRequest per model call");
    assert!(String::from_utf8_lossy(&requests[1]).contains("again"));
    let home = g.root.join("scratch/agent/home");
    assert_eq!(
        fs::read(home.join("reply1.json")).unwrap(),
        b"{\"reply\":1}"
    );
    assert_eq!(
        fs::read(home.join("reply2.json")).unwrap(),
        b"{\"reply\":2}"
    );
    let Outcome::Done {
        exit_code,
        signal,
        timed_out,
        patch,
        workspace_digest,
    } = outcome
    else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    assert_eq!((exit_code, signal, timed_out), (Some(0), None, false));
    // The digest is of the scratch tree the patch was cut from, after the purge.
    let tree = g.root.join("scratch/agent/work");
    assert_eq!(workspace_digest, workspace_digest_of(&tree));
    let patch = String::from_utf8(patch).unwrap();
    assert!(
        patch.contains("hello.txt") && patch.contains("+edited"),
        "{patch}"
    );
    // The real workspace is never touched.
    assert_eq!(
        fs::read(g.root.join("workspace/hello.txt")).unwrap(),
        b"original\n"
    );
}

#[test]
fn the_patch_has_the_edits_and_new_files_but_no_bytecode_or_caches_and_applies() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let script = "set -eu\n\
        printf 'edited\\n' > hello.txt\n\
        printf 'new\\n' > added.txt\n\
        mkdir -p __pycache__ .pytest_cache sub\n\
        printf x > __pycache__/m.cpython-314.pyc\n\
        printf x > sub/n.pyc\n\
        printf x > .pytest_cache/v\n";
    let cli = write_cli(&g, script);
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    let Outcome::Done {
        patch, exit_code, ..
    } = outcome
    else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    assert_eq!(exit_code, Some(0));
    let text = String::from_utf8(patch.clone()).unwrap();
    assert!(
        text.contains("+++ b/added.txt") && text.contains("+++ b/hello.txt"),
        "{text}"
    );
    for banned in ["__pycache__", ".pyc", ".pytest_cache"] {
        assert!(!text.contains(banned), "{banned} in the patch:\n{text}");
    }
    // The same patch applies through the existing ApplyPatch path to a copy of the original.
    let copy = tempfile::tempdir().unwrap();
    let mut backend = agentos_guest::FakeBackend::new(copy.path()).unwrap();
    fs::create_dir_all(copy.path().join("workspace")).unwrap();
    fs::write(copy.path().join("workspace/hello.txt"), b"original\n").unwrap();
    let editable = ["hello.txt".to_string(), "added.txt".to_string()];
    agentos_guest::handlers::apply_patch(&mut backend, base, &editable, &patch).unwrap();
    assert_eq!(
        fs::read(copy.path().join("workspace/hello.txt")).unwrap(),
        b"edited\n"
    );
    assert_eq!(
        fs::read(copy.path().join("workspace/added.txt")).unwrap(),
        b"new\n"
    );
    assert!(!copy.path().join("workspace/__pycache__").exists());
}

#[test]
fn a_cli_that_never_exits_is_killed_at_its_deadline_with_its_whole_group() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let script = "/bin/sleep 1000 &\n\
        echo $! > \"$HOME/sleeper.pid\"\n\
        echo $$ > \"$HOME/leader.pid\"\n\
        wait\n";
    let cli = write_cli(&g, script);
    let started = Instant::now();
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        1,
        base,
        answer_json(&mut n),
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let Outcome::Done {
        exit_code,
        signal,
        timed_out,
        patch,
        ..
    } = outcome
    else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    assert_eq!((exit_code, signal, timed_out), (None, Some(9), true));
    assert!(patch.is_empty());
    let home = g.root.join("scratch/agent/home");
    for file in ["leader.pid", "sleeper.pid"] {
        let pid = fs::read_to_string(home.join(file)).unwrap();
        let pid = pid.trim();
        let deadline = Instant::now() + Duration::from_secs(2);
        while is_running(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!is_running(pid), "{file} ({pid}) survived the kill");
    }
}

#[test]
fn a_wrong_expected_base_is_refused_and_nothing_runs() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let cli = write_cli(&g, "printf ran > \"$HOME/ran\"\n");
    let mut n = 0;
    let (requests, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        Digest::of(b"not the base"),
        answer_json(&mut n),
    );
    assert!(requests.is_empty());
    let Outcome::Refused(reason) = outcome else {
        panic!("expected Refused, got {outcome:?}")
    };
    assert!(
        reason.starts_with("version conflict: expected "),
        "{reason}"
    );
    assert!(!g.root.join("scratch/agent/home/ran").exists());
}

#[test]
fn a_binary_change_is_refused_with_the_patchrules_reason() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("data.bin", &[0u8, 1, 2, 3])]);
    let cli = write_cli(&g, "printf 'x\\000y' > data.bin\n");
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    let Outcome::Refused(reason) = outcome else {
        panic!("expected Refused, got {outcome:?}")
    };
    assert_eq!(reason, "binary patches are not supported: data.bin");
}

#[test]
fn the_message_cannot_override_the_proxy_key_or_the_session_variables() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let cli = write_cli(&g, "/usr/bin/env > \"$HOME/env.txt\"\n");
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[
            ("ANTHROPIC_API_KEY", "sk-real-secret"),
            ("ANTHROPIC_BASE_URL", "http://203.0.113.9"),
            ("HOME", "/etc"),
            ("FOO", "bar"),
        ],
        20,
        base,
        answer_json(&mut n),
    );
    assert!(
        matches!(
            outcome,
            Outcome::Done {
                exit_code: Some(0),
                ..
            }
        ),
        "{outcome:?}"
    );
    let home = g.root.join("scratch/agent/home");
    let env = fs::read_to_string(home.join("env.txt")).unwrap();
    assert!(
        !env.contains("sk-real-secret") && !env.contains("203.0.113.9"),
        "{env}"
    );
    assert!(
        env.lines().any(|l| l == "ANTHROPIC_API_KEY=placeholder"),
        "{env}"
    );
    assert!(
        env.lines()
            .any(|l| l.starts_with("ANTHROPIC_BASE_URL=http://127.0.0.1:")),
        "{env}"
    );
    assert!(
        env.lines().any(|l| l == format!("HOME={}", home.display())),
        "{env}"
    );
    assert!(env.lines().any(|l| l == "FOO=bar"), "{env}");
    assert!(
        env.lines().any(|l| l == "PYTHONDONTWRITEBYTECODE=1"),
        "{env}"
    );
    assert!(env.lines().any(|l| l == "DISABLE_TELEMETRY=1"), "{env}");
    assert!(env.lines().any(|l| l == "DISABLE_AUTOUPDATER=1"), "{env}");
    assert!(
        env.lines()
            .any(|l| l == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"),
        "{env}"
    );
}

#[test]
fn a_cli_that_changes_nothing_gets_an_empty_patch() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let cli = write_cli(&g, "exit 0\n");
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    let Outcome::Done {
        exit_code, patch, ..
    } = outcome
    else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    assert_eq!(exit_code, Some(0));
    assert!(patch.is_empty());
}

#[test]
fn a_relative_argv0_is_refused() {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, &[("hello.txt", b"original\n")]);
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["sh", "-c", "true"],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    let Outcome::Refused(reason) = outcome else {
        panic!("expected Refused, got {outcome:?}")
    };
    assert_eq!(reason, "argv[0] must be an absolute path: sh");
}

/// Runs `script` against a snapshot of `files` and returns the outcome and the patch.
fn cut_for(files: &[(&str, &[u8])], script: &str) -> (FakeGuest, Digest, Outcome) {
    let g = spawn();
    let mut s = g.hello(TOKEN, Mode::Job);
    let base = snapshot(&mut s, files);
    let cli = write_cli(&g, script);
    let mut n = 0;
    let (_, outcome) = run_agent(
        &mut s,
        &["/bin/sh", &cli],
        &[],
        20,
        base,
        answer_json(&mut n),
    );
    (g, base, outcome)
}

#[test]
fn a_moved_file_is_a_deletion_and_a_creation_not_a_rename() {
    let (_g, _base, outcome) = cut_for(&[("old.txt", b"same contents\n")], "mv old.txt new.txt\n");
    let Outcome::Done { patch, .. } = outcome else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    let text = String::from_utf8(patch).unwrap();
    assert!(text.contains("deleted file mode 100644"), "{text}");
    assert!(text.contains("new file mode 100644"), "{text}");
    assert!(!text.contains("rename"), "{text}");
}

#[test]
fn a_gitignore_in_the_workspace_does_not_hide_the_cli_files_from_the_patch() {
    let (_g, _base, outcome) = cut_for(&[(".gitignore", b"*.log\n")], "printf 'x\\n' > run.log\n");
    let Outcome::Done { patch, .. } = outcome else {
        panic!("expected AgentDone, got {outcome:?}")
    };
    let text = String::from_utf8(patch).unwrap();
    assert!(text.contains("+++ b/run.log"), "{text}");
}

#[test]
fn a_symlink_made_by_the_cli_is_refused() {
    let (_g, _base, outcome) = cut_for(&[("hello.txt", b"original\n")], "ln -s hello.txt link\n");
    let Outcome::Refused(reason) = outcome else {
        panic!("expected Refused, got {outcome:?}")
    };
    assert!(
        reason.starts_with("cannot digest the agent's tree: symlink in workspace"),
        "{reason}"
    );
}
