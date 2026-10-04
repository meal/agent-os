//! The VM agent is PID 1: every orphan of the check is re-parented to it. This binary makes
//! itself a child subreaper to stand in for that (its own process, so no other test's
//! orphans are affected), then checks that a verification leaves no zombie behind.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;

use agentos_core::guest::{Frame, Message, RAW_FRAME_LIMIT, write_frame};
use agentos_core::workspace::list_files;
use agentos_guest::backend::FakeBackend;
use agentos_guest::handlers::{self, StagedProfile};

/// Our children in state `Z`.
fn zombie_children() -> Vec<String> {
    let me = std::process::id().to_string();
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid …`; comm may hold spaces, so split after the last ')'.
        let Some(rest) = stat.rsplit_once(") ").map(|(_, r)| r) else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() > 1 && f[0] == "Z" && f[1] == me {
            found.push(stat.trim().to_string());
        }
    }
    found
}

fn snapshot(backend: &mut FakeBackend) {
    let tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/parser-repo");
    let files: Vec<(String, Vec<u8>)> = list_files(&tree)
        .unwrap()
        .into_iter()
        .map(|(r, p)| (r, fs::read(p).unwrap()))
        .collect();
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    let count = files.len() as u64;
    let (mut host, mut guest) = UnixStream::pair().unwrap();
    let writer = thread::spawn(move || {
        for (path, bytes) in &files {
            write_frame(
                &mut host,
                &Frame::Json(Message::File {
                    path: path.clone(),
                    len: bytes.len() as u64,
                }),
            )
            .unwrap();
            for chunk in bytes.chunks(RAW_FRAME_LIMIT) {
                write_frame(&mut host, &Frame::Raw(chunk.to_vec())).unwrap();
            }
        }
        write_frame(&mut host, &Frame::Json(Message::EndFiles)).unwrap();
    });
    handlers::read_snapshot(backend, &mut guest, count, total).unwrap();
    writer.join().unwrap();
}

fn sh_profile(script: &str) -> StagedProfile {
    let json =
        serde_json::json!({ "id": "sh", "command": ["sh", "-c", script], "protected": true })
            .to_string();
    StagedProfile {
        files: vec![("profile.json".into(), json.into_bytes())],
    }
}

#[test]
fn the_checks_orphans_are_reaped_after_the_run_and_after_a_timeout() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut b = FakeBackend::new(dir.path().join("root")).unwrap();
    snapshot(&mut b);

    // Background members outlive the leader; the group kill orphans them to us.
    let v =
        handlers::run_verification(&mut b, None, 30, sh_profile("sleep 30 & sleep 30 & exit 0"))
            .unwrap();
    assert_eq!(v.exit_code, Some(0));
    assert_eq!(zombie_children(), Vec::<String>::new());

    let err = handlers::run_verification(
        &mut b,
        None,
        1,
        sh_profile("sh -c 'sleep 30 & sleep 30' & sleep 30"),
    )
    .unwrap_err();
    assert_eq!(err, "timeout");
    assert_eq!(zombie_children(), Vec::<String>::new());
}
