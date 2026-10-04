//! The guest's request handlers over a `FakeBackend`: results and refusal reasons must be
//! the host worker's (3a `FixtureExecutor`), byte for byte.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::guest::{
    Frame, Message, OUTPUT_LIMIT, PatchStateKind, RAW_FRAME_LIMIT, unb64, write_frame,
};
use agentos_core::ids::Digest;
use agentos_core::workspace::{list_files, workspace_digest};
use agentos_guest::backend::{Backend, FakeBackend};
use agentos_guest::handlers::{self, StagedProfile, StreamError};
use rustix::process::{Pid, test_kill_process_group};

const BASE: &str = "be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8";
const FIXED: &str = "060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn digest(hex: &str) -> Digest {
    Digest::from_hex(hex).unwrap()
}

fn fix_patch() -> Vec<u8> {
    fs::read(fixtures().join("parser-repo.fix.patch")).unwrap()
}

fn src() -> Vec<String> {
    vec!["src/**".to_string()]
}

struct Guest {
    dir: tempfile::TempDir,
    backend: FakeBackend,
}

impl Guest {
    fn new() -> Guest {
        let dir = tempfile::tempdir().unwrap();
        let backend = FakeBackend::new(dir.path().join("root")).unwrap();
        Guest { dir, backend }
    }

    fn ws(&self) -> &Path {
        self.backend.workspace_dir()
    }

    /// Streams `tree` to the guest exactly as the host would: `File{path, len}` and its raw
    /// frames per file, then `EndFiles`. Returns what `read_snapshot` returned.
    fn snapshot_of(&mut self, tree: &Path) -> Result<(Vec<String>, Digest), StreamError> {
        let files: Vec<(String, Vec<u8>)> = list_files(tree)
            .unwrap()
            .into_iter()
            .map(|(rel, p)| (rel, fs::read(p).unwrap()))
            .collect();
        let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        let count = files.len() as u64;
        let writer = thread::spawn(move || send_files(&mut host, &files));
        let result = handlers::read_snapshot(&mut self.backend, &mut guest, count, total);
        writer.join().unwrap();
        result
    }

    fn snapshot(&mut self) {
        let (_, d) = self.snapshot_of(&fixtures().join("parser-repo")).unwrap();
        assert_eq!(d, digest(BASE));
    }

    fn apply(&mut self, base: Digest, patch: &[u8]) -> Result<(Vec<String>, Digest), String> {
        handlers::apply_patch(&mut self.backend, base, &src(), patch)
    }

    fn verify(
        &mut self,
        pinned: Option<Digest>,
        timeout: u64,
        profile: &StagedProfile,
    ) -> Result<handlers::Verified, String> {
        handlers::run_verification(&mut self.backend, pinned, timeout, profile.clone())
    }
}

fn send_files(host: &mut UnixStream, files: &[(String, Vec<u8>)]) {
    for (path, bytes) in files {
        let _ = write_frame(
            host,
            &Frame::Json(Message::File {
                path: path.clone(),
                len: bytes.len() as u64,
            }),
        );
        for chunk in bytes.chunks(RAW_FRAME_LIMIT) {
            let _ = write_frame(host, &Frame::Raw(chunk.to_vec()));
        }
    }
    let _ = write_frame(host, &Frame::Json(Message::EndFiles));
}

fn profile(json: &str) -> StagedProfile {
    StagedProfile {
        files: vec![("profile.json".into(), json.as_bytes().to_vec())],
    }
}

fn sh_profile(script: &str) -> StagedProfile {
    let command = serde_json::json!(["sh", "-c", script]);
    profile(&serde_json::json!({ "id": "sh", "command": command, "protected": true }).to_string())
}

fn fixture_profile() -> StagedProfile {
    let dir = fixtures().join("profiles/parser-checks-v1");
    StagedProfile {
        files: list_files(&dir)
            .unwrap()
            .into_iter()
            .map(|(rel, p)| (rel, fs::read(p).unwrap()))
            .collect(),
    }
}

#[test]
fn read_snapshot_writes_the_files_and_reports_the_host_digest() {
    let mut g = Guest::new();
    let (files, d) = g.snapshot_of(&fixtures().join("parser-repo")).unwrap();
    assert_eq!(d, digest(BASE));
    let expected: Vec<String> = list_files(&fixtures().join("parser-repo"))
        .unwrap()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(files, expected);
    assert_eq!(workspace_digest(g.ws()).unwrap(), digest(BASE));
    // A retry starts from a clean workspace.
    fs::write(g.ws().join("stray.txt"), "x").unwrap();
    let (_, again) = g.snapshot_of(&fixtures().join("parser-repo")).unwrap();
    assert_eq!(again, digest(BASE));
    assert!(!g.ws().join("stray.txt").exists());
}

#[test]
fn read_snapshot_splits_a_large_file_across_raw_frames() {
    let mut g = Guest::new();
    let tree = g.dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("big.bin"), vec![7u8; RAW_FRAME_LIMIT + 3]).unwrap();
    let (files, d) = g.snapshot_of(&tree).unwrap();
    assert_eq!(files, ["big.bin"]);
    assert_eq!(d, workspace_digest(&tree).unwrap());
}

#[test]
fn read_snapshot_stream_violations_are_protocol_errors() {
    // (announced count, announced bytes, frames) ⇒ protocol violation
    let cases: Vec<(&str, u64, u64, Vec<Frame>)> = vec![
        ("too many files announced", 65_537, 0, vec![]),
        ("too many bytes announced", 1, (256 << 20) + 1, vec![]),
        (
            "file over the limit",
            1,
            1 << 30,
            vec![Frame::Json(Message::File {
                path: "a".into(),
                len: (64 << 20) + 1,
            })],
        ),
        (
            "more bytes than announced",
            1,
            2,
            vec![
                Frame::Json(Message::File {
                    path: "a".into(),
                    len: 3,
                }),
                Frame::Raw(b"abc".to_vec()),
            ],
        ),
        (
            "more files than announced",
            1,
            2,
            vec![
                Frame::Json(Message::File {
                    path: "a".into(),
                    len: 1,
                }),
                Frame::Raw(b"a".to_vec()),
                Frame::Json(Message::File {
                    path: "b".into(),
                    len: 1,
                }),
                Frame::Raw(b"b".to_vec()),
            ],
        ),
        (
            "fewer files than announced",
            2,
            1,
            vec![
                Frame::Json(Message::File {
                    path: "a".into(),
                    len: 1,
                }),
                Frame::Raw(b"a".to_vec()),
                Frame::Json(Message::EndFiles),
            ],
        ),
        (
            "short raw frame",
            1,
            3,
            vec![
                Frame::Json(Message::File {
                    path: "a".into(),
                    len: 3,
                }),
                Frame::Raw(b"ab".to_vec()),
            ],
        ),
        (
            "traversal path",
            1,
            1,
            vec![
                Frame::Json(Message::File {
                    path: "../a".into(),
                    len: 1,
                }),
                Frame::Raw(b"a".to_vec()),
            ],
        ),
        (
            "absolute path",
            1,
            1,
            vec![
                Frame::Json(Message::File {
                    path: "/a".into(),
                    len: 1,
                }),
                Frame::Raw(b"a".to_vec()),
            ],
        ),
        ("wrong message", 1, 1, vec![Frame::Json(Message::Shutdown)]),
        ("eof", 1, 1, vec![]),
    ];
    for (name, count, total, frames) in cases {
        let mut g = Guest::new();
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for f in &frames {
                let _ = write_frame(&mut host, f);
            }
            drop(host);
        });
        let got = handlers::read_snapshot(&mut g.backend, &mut guest, count, total);
        writer.join().unwrap();
        assert!(
            matches!(got, Err(StreamError::Protocol(_))),
            "{name}: {got:?}"
        );
    }
}

#[test]
fn apply_patch_reports_paths_and_the_new_digest_matching_the_host_worker() {
    let mut g = Guest::new();
    g.snapshot();
    let (paths, d) = g.apply(digest(BASE), &fix_patch()).unwrap();
    assert_eq!(paths, ["src/parser.py"]);
    assert_eq!(d, digest(FIXED));
    assert_eq!(workspace_digest(g.ws()).unwrap(), digest(FIXED));
}

#[test]
fn refused_reasons_match_the_3a_table() {
    let base = digest(BASE);
    let create = |path: &str| {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x = 1\n"
        )
    };
    let rename = "diff --git a/tests/test_parser.py b/src/test_parser.py\nsimilarity index 100%\nrename from tests/test_parser.py\nrename to src/test_parser.py\n";
    // `git diff --cached --binary` of a new 3-byte file.
    let binary = "diff --git a/src/b.bin b/src/b.bin\nnew file mode 100644\nindex 0000000000000000000000000000000000000000..8352675d67aed6625ece79af41c27fdb4ee2e867\nGIT binary patch\nliteral 3\nKcmZQzWC8#H2LJ>B\n\nliteral 0\nHcmV?d00001\n\n";
    let stale = "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1,2 +1,2 @@\n-this line is not there\n+nor is this one\n context\n";

    // No snapshot at all.
    let mut g = Guest::new();
    assert_eq!(
        g.apply(base, &fix_patch()).unwrap_err(),
        "workspace missing: no snapshot was read"
    );
    assert_eq!(
        g.verify(None, 5, &sh_profile("true")).unwrap_err(),
        "workspace missing: no snapshot was read"
    );

    // (patch, expected base, reason prefix or exact reason, exact?)
    let cases: Vec<(String, Digest, String, bool)> = vec![
        (
            "this is not a patch\n".into(),
            base,
            "invalid patch: ".into(),
            false,
        ),
        (
            rename.into(),
            base,
            "unsupported patch operation: rename".into(),
            false,
        ),
        (
            binary.into(),
            base,
            "binary patches are not supported: src/b.bin".into(),
            true,
        ),
        // An empty patch never reaches the numstat parser: git itself refuses it, in 3a too.
        (
            String::new(),
            base,
            "invalid patch: error: No valid patches in input".into(),
            false,
        ),
        (
            create("tests/x.py"),
            base,
            "path not editable: tests/x.py".into(),
            true,
        ),
        (
            create("src/__pycache__/x.py"),
            base,
            "path excluded from the workspace digest: src/__pycache__/x.py".into(),
            true,
        ),
        (
            fs::read_to_string(fixtures().join("parser-repo.fix.patch")).unwrap(),
            Digest::of(b"other"),
            format!(
                "version conflict: expected {}, actual {BASE}",
                Digest::of(b"other")
            ),
            true,
        ),
        (stale.into(), base, "patch does not apply: ".into(), false),
    ];
    for (patch, expected_base, want, exact) in cases {
        let mut g = Guest::new();
        g.snapshot();
        let got = g.apply(expected_base, patch.as_bytes()).unwrap_err();
        if exact {
            assert_eq!(got, want);
        } else {
            assert!(got.starts_with(&want), "want {want:?}…, got {got:?}");
        }
        assert_eq!(
            workspace_digest(g.ws()).unwrap(),
            base,
            "a refused patch changes nothing: {want}"
        );
    }

    // A symlink on the patched path is reported before the digest (which a symlink breaks).
    let mut g = Guest::new();
    g.snapshot();
    std::os::unix::fs::symlink(g.dir.path(), g.ws().join("src/link")).unwrap();
    assert_eq!(
        g.apply(base, create("src/link/x.py").as_bytes())
            .unwrap_err(),
        "path src/link/x.py crosses symlink src/link"
    );
}

#[test]
fn a_planted_git_dir_is_purged_before_git_apply() {
    let mut g = Guest::new();
    g.snapshot();
    fs::create_dir_all(g.ws().join(".git")).unwrap();
    fs::write(g.ws().join(".git/config"), "[core]\n\tworktree = /\n").unwrap();
    let (_, d) = g.apply(digest(BASE), &fix_patch()).unwrap();
    assert_eq!(d, digest(FIXED));
    assert!(!g.ws().join(".git").exists());
}

#[test]
fn run_verification_passes_and_fails_with_the_3a_evidence_fields() {
    let prof = fixture_profile();
    let profile_digest = workspace_digest(&fixtures().join("profiles/parser-checks-v1")).unwrap();

    let mut g = Guest::new();
    g.snapshot();
    let failing = g.verify(Some(profile_digest), 30, &prof).unwrap();
    assert_eq!(failing.exit_code, Some(1));
    assert_eq!(failing.workspace_digest, digest(BASE));

    g.apply(digest(BASE), &fix_patch()).unwrap();
    let v = g.verify(Some(profile_digest), 30, &prof).unwrap();
    assert_eq!(v.exit_code, Some(0));
    assert_eq!(v.profile_id, "parser-checks-v1");
    assert_eq!(v.command, ["python3", "check_parser.py"]);
    assert_eq!(v.profile_digest, profile_digest);
    assert_eq!(v.workspace_digest, digest(FIXED));
    assert_eq!(
        String::from_utf8_lossy(&v.stdout).trim(),
        "10/10 checks passed"
    );
    assert!(!v.stdout_truncated && !v.stderr_truncated);

    // Unpinned runs work too, and the bytes survive the trip through the message.
    let v = g
        .verify(
            None,
            30,
            &sh_profile("printf 'out\\n'; printf 'err\\377' >&2; exit 3"),
        )
        .unwrap();
    assert_eq!(v.exit_code, Some(3));
    let Message::Verified {
        stdout_b64,
        stderr_b64,
        exit_code,
        profile_id,
        ..
    } = v.clone().into_message()
    else {
        panic!()
    };
    assert_eq!(unb64(&stdout_b64).unwrap(), b"out\n");
    assert_eq!(unb64(&stderr_b64).unwrap(), b"err\xff");
    assert_eq!((exit_code, profile_id.as_str()), (Some(3), "sh"));

    // A signal-killed check has no exit code, as in 3a.
    let v = g.verify(None, 30, &sh_profile("kill -9 $$")).unwrap();
    assert_eq!(v.exit_code, None);
}

#[test]
fn profile_json_problems_are_refused_with_the_3a_reasons() {
    let mut g = Guest::new();
    g.snapshot();
    assert!(
        g.verify(None, 5, &profile("{nope"))
            .unwrap_err()
            .starts_with("invalid profile.json: ")
    );
    assert_eq!(
        g.verify(None, 5, &profile(r#"{"id":"x","command":[]}"#))
            .unwrap_err(),
        "profile command is empty"
    );
    assert!(
        g.verify(
            None,
            5,
            &profile(r#"{"id":"x","command":["/nonexistent/prog"]}"#)
        )
        .unwrap_err()
        .starts_with("cannot run profile command: ")
    );
}

#[test]
fn verification_timeout_kills_the_group_and_is_refused_timeout() {
    let mut g = Guest::new();
    g.snapshot();
    let pidfile = g.dir.path().join("pgid");
    let script = format!("echo $$ > {}; sleep 30 & sleep 30", pidfile.display());
    let started = Instant::now();
    assert_eq!(
        g.verify(None, 1, &sh_profile(&script)).unwrap_err(),
        "timeout"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let pgid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pgid = Pid::from_raw(pgid).unwrap();
    // Orphaned members are reaped by the container's init; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(3);
    while test_kill_process_group(pgid).is_ok() {
        assert!(
            Instant::now() < deadline,
            "a member of group {pgid:?} survived the timeout"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn pinned_profile_mismatch_is_refused_before_running() {
    let mut g = Guest::new();
    g.snapshot();
    let marker = g.dir.path().join("marker");
    let prof = sh_profile(&format!("touch {}", marker.display()));
    let pinned = Digest::of(b"pinned");
    let err = g.verify(Some(pinned), 5, &prof).unwrap_err();
    // The staged copy's digest is what the guest reports as "found".
    let staged = g.backend.scratch_dir().join("profile");
    assert_eq!(
        err,
        format!(
            "profile digest mismatch: pinned {pinned}, found {}",
            workspace_digest(&staged).unwrap()
        )
    );
    assert!(!marker.exists());
}

#[test]
fn oversized_output_sets_the_truncation_flags_at_64_kib_plus_one() {
    let mut g = Guest::new();
    g.snapshot();
    let script = format!(
        "head -c {} /dev/zero | tr '\\0' a; head -c {} /dev/zero | tr '\\0' b >&2",
        OUTPUT_LIMIT + 1,
        OUTPUT_LIMIT
    );
    let v = g.verify(None, 30, &sh_profile(&script)).unwrap();
    assert_eq!((v.stdout.len(), v.stdout_truncated), (OUTPUT_LIMIT, true));
    assert_eq!((v.stderr.len(), v.stderr_truncated), (OUTPUT_LIMIT, false));
    assert!(v.stdout.iter().all(|b| *b == b'a'));

    // Far more than the limit is drained without blocking the check.
    let v = g
        .verify(None, 30, &sh_profile("head -c 10000000 /dev/zero"))
        .unwrap();
    assert_eq!(
        (v.exit_code, v.stdout.len(), v.stdout_truncated),
        (Some(0), OUTPUT_LIMIT, true)
    );
}

#[test]
fn a_check_that_pollutes_the_workspace_voids_its_evidence() {
    let mut g = Guest::new();
    g.snapshot();
    let err = g
        .verify(
            None,
            30,
            &sh_profile("mkdir -p \"$0/src/__pycache__\" && touch \"$0/src/__pycache__/x.pyc\""),
        )
        .unwrap_err();
    assert_eq!(
        err,
        "workspace polluted by excluded entries: src/__pycache__"
    );

    let err = g
        .verify(None, 30, &sh_profile("echo more >> \"$0/src/parser.py\""))
        .unwrap_err();
    assert_eq!(err, "workspace changed during verification");

    let err = g
        .verify(None, 30, &sh_profile("echo '{}' > profile.json"))
        .unwrap_err();
    assert_eq!(err, "protected profile changed during verification");
}

#[test]
fn a_bytecode_cache_left_in_the_workspace_is_purged_before_the_check() {
    let mut g = Guest::new();
    g.snapshot();
    fs::create_dir_all(g.ws().join("src/__pycache__")).unwrap();
    fs::write(
        g.ws().join("src/__pycache__/parser.cpython-311.pyc"),
        "stale",
    )
    .unwrap();
    let v = g
        .verify(None, 30, &sh_profile("test ! -e \"$0/src/__pycache__\""))
        .unwrap();
    assert_eq!(v.exit_code, Some(0));
}

#[test]
fn digest_and_patch_state_trichotomy() {
    let base = digest(BASE);
    let g0 = Guest::new();
    assert!(handlers::digest(&g0.backend).is_err());
    let missing = handlers::patch_state(&g0.backend, base, &fix_patch());
    assert_eq!(missing.state, PatchStateKind::Unknown);

    let mut g = Guest::new();
    g.snapshot();
    assert_eq!(handlers::digest(&g.backend).unwrap(), base);
    let s = handlers::patch_state(&g.backend, base, &fix_patch());
    assert_eq!(s.state, PatchStateKind::NotApplied, "{s:?}");

    g.apply(base, &fix_patch()).unwrap();
    let s = handlers::patch_state(&g.backend, base, &fix_patch());
    assert_eq!(
        (
            s.state,
            s.paths.clone(),
            s.workspace_digest,
            s.reason.clone()
        ),
        (
            PatchStateKind::Applied,
            vec!["src/parser.py".to_string()],
            Some(digest(FIXED)),
            None
        )
    );
    let Message::PatchStateIs { state, .. } = s.into_message() else {
        panic!()
    };
    assert_eq!(state, PatchStateKind::Applied);
    // Inspection never changes the workspace.
    assert_eq!(workspace_digest(g.ws()).unwrap(), digest(FIXED));

    fs::write(g.ws().join("src/extra.py"), "tampered\n").unwrap();
    let actual = workspace_digest(g.ws()).unwrap();
    let s = handlers::patch_state(&g.backend, base, &fix_patch());
    assert_eq!(s.state, PatchStateKind::Unknown);
    assert_eq!(
        s.reason.unwrap(),
        format!("workspace {actual} is neither the base {base} nor the base with this patch")
    );
    assert_eq!(handlers::digest(&g.backend).unwrap(), actual);
}

#[test]
fn the_bytecode_cache_is_fresh_for_every_verification_in_one_boot() {
    let mut g = Guest::new();
    g.snapshot();
    // Each run reports what it found under the check's own scratch, then leaves something.
    let script = "ls -A \"$PYTHONPYCACHEPREFIX\" 2>/dev/null | wc -l; ls -A \"$PYTHONPYCACHEPREFIX/..\" | wc -l; \
                  mkdir -p \"$PYTHONPYCACHEPREFIX\" && touch \"$PYTHONPYCACHEPREFIX/stale.pyc\" \"$PYTHONPYCACHEPREFIX/../junk\"";
    for run in 0..2 {
        let v = g.verify(None, 30, &sh_profile(script)).unwrap();
        assert_eq!(v.exit_code, Some(0), "run {run}: {v:?}");
        let counts: Vec<String> = String::from_utf8_lossy(&v.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(
            counts,
            ["0", "0"],
            "run {run} saw a previous run's leftovers"
        );
    }
    // The check's directory itself stays (in a VM it is 0700 and owned by `check`).
    assert!(g.backend.scratch_dir().join("check").is_dir());
}

/// `sh -c script` with `filler` split over extra arguments (one argument may not exceed
/// 128 KiB at `execve`).
fn padded_profile(script: &str, filler: &str) -> StagedProfile {
    let mut command = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
    ];
    let chars: Vec<char> = filler.chars().collect();
    command.extend(chars.chunks(100_000).map(|c| c.iter().collect::<String>()));
    profile(
        &serde_json::json!({ "id": "padded", "command": command, "protected": true }).to_string(),
    )
}

#[test]
fn a_profile_command_too_large_for_the_verified_frame_is_refused_before_running() {
    let mut g = Guest::new();
    g.snapshot();
    let marker = g.dir.path().join("ran");
    let touch = format!("touch {}", marker.display());
    // Plain bytes that JSON leaves alone, then control bytes that JSON escapes sixfold.
    for filler in ["a".repeat(900_000), "\u{1}".repeat(150_000)] {
        let err = g
            .verify(None, 30, &padded_profile(&touch, &filler))
            .unwrap_err();
        assert!(err.starts_with("profile command too large: "), "{err}");
        assert!(!marker.exists(), "the oversized command ran");
    }
    // Just under the budget still runs, and its Verified frame fits with full output.
    let script = format!("{touch}; head -c 70000 /dev/zero; head -c 70000 /dev/zero >&2");
    let v = g
        .verify(None, 30, &padded_profile(&script, &"a".repeat(800_000)))
        .unwrap();
    assert!(marker.exists());
    assert!(v.stdout_truncated && v.stderr_truncated);
    let mut buf = Vec::new();
    write_frame(&mut buf, &Frame::Json(v.into_message()))
        .expect("the Verified reply fits in one JSON frame");
}

/// A `FakeBackend` that records which trees the handlers hand to `own_tree`.
struct Recording {
    inner: FakeBackend,
    owned: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

impl Backend for Recording {
    fn workspace_dir(&self) -> &Path {
        self.inner.workspace_dir()
    }
    fn scratch_dir(&self) -> &Path {
        self.inner.scratch_dir()
    }
    fn prepare_workspace(&mut self) -> Result<(), String> {
        self.inner.prepare_workspace()
    }
    fn sync_workspace(&self) -> Result<(), String> {
        self.inner.sync_workspace()
    }
    fn remount_workspace_ro(&self) -> Result<(), String> {
        self.inner.remount_workspace_ro()
    }
    fn own_tree(&self, dir: &Path) -> Result<(), String> {
        self.owned.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }
    fn git(&self, cwd: &Path) -> std::process::Command {
        self.inner.git(cwd)
    }
    fn check_command(
        &self,
        program: &str,
        args: &[String],
        workspace: &Path,
        cwd: &Path,
        pycache: &Path,
    ) -> std::process::Command {
        self.inner
            .check_command(program, args, workspace, cwd, pycache)
    }
    fn vcpus(&self) -> u32 {
        1
    }
    fn memory_mib(&self) -> u32 {
        1
    }
}

#[test]
fn trees_written_as_root_are_handed_to_the_builder_before_git_touches_them() {
    let dir = tempfile::tempdir().unwrap();
    let owned = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut b = Recording {
        inner: FakeBackend::new(dir.path().join("root")).unwrap(),
        owned: owned.clone(),
    };
    let files: Vec<(String, Vec<u8>)> = list_files(&fixtures().join("parser-repo"))
        .unwrap()
        .into_iter()
        .map(|(rel, p)| (rel, fs::read(p).unwrap()))
        .collect();
    let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
    let count = files.len() as u64;
    let (mut host, mut guest) = UnixStream::pair().unwrap();
    let writer = thread::spawn(move || send_files(&mut host, &files));
    handlers::read_snapshot(&mut b, &mut guest, count, total).unwrap();
    writer.join().unwrap();
    assert_eq!(*owned.lock().unwrap(), [b.workspace_dir().to_path_buf()]);

    owned.lock().unwrap().clear();
    handlers::apply_patch(&mut b, digest(BASE), &src(), &fix_patch()).unwrap();
    // The reverse check runs git on a root-made copy under the scratch drive.
    let s = handlers::patch_state(&b, digest(BASE), &fix_patch());
    assert_eq!(s.state, PatchStateKind::Applied, "{s:?}");
    assert_eq!(*owned.lock().unwrap(), [b.scratch_dir().join("reverse/ws")]);
}
