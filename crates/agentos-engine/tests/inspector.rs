//! Inspection boots: `Inspector` (the controller's own VM, booted in inspect mode over the
//! task's `ws.img`), `Reconciler::Firecracker`, the per-job attempt tokens and the
//! controller's collection of a settled job's jail. Fake guest and fake jailer; no KVM.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::guest::{is_attempt_token, PatchStateKind};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::crash::{CrashHook, CrashPoint};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use agentos_engine::firecracker::{Answer, FirecrackerConfig, FirecrackerWorker, Inspector, Query, INSPECT_TIMEOUT};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::jail::JailMode;
use agentos_engine::job::{JobDir, JobRequest, WorkerConfig};
use agentos_engine::supervised::{ExecCounts, Reconciler};
use agentos_engine::worker::Worker;
use agentos_engine::workspace::workspace_digest;
use common::{
    contract, copy_dir, fake_firecracker_config, fix_patch, fixtures, jailed_fake_firecracker_config, supervised,
};
use rustix::process::{kill_process, Pid, Signal};
use tempfile::TempDir;

const TEST_WORKERS: &str = "AGENTOS_TEST_WORKERS";
const NEVER_LISTEN: &str = "AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN";
const HANG_INSPECT: &str = "AGENTOS_TEST_FAKE_GUEST_HANG_INSPECT";
const KILL_VM_AFTER_REQUEST: &str = "AGENTOS_TEST_KILL_VM_AFTER_REQUEST";
/// Upper bound for anything a test waits on.
const PATIENCE: Duration = Duration::from_secs(20);

struct Fx {
    dir: TempDir,
    cfg: FirecrackerConfig,
    task: TaskId,
    contract: Contract,
}

fn test_env() -> Vec<(String, String)> {
    vec![(TEST_WORKERS.into(), "1".into())]
}

fn env_with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut env = test_env();
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    env
}

fn ctx() -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "test".into() }
}

impl Fx {
    fn new() -> Fx {
        Fx::build(false)
    }

    fn jailed() -> Fx {
        Fx::build(true)
    }

    fn build(jailed: bool) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let cfg = if jailed { jailed_fake_firecracker_config(dir.path()) } else { fake_firecracker_config(dir.path()) };
        Fx { dir, cfg, task: TaskId::new(), contract: contract(10).0 }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn task_dir(&self) -> PathBuf {
        self.path("work").join(self.task.as_str())
    }

    fn ws_img(&self) -> PathBuf {
        self.task_dir().join("ws.img")
    }

    /// The fake guest's view of the workspace drive.
    fn guest_workspace(&self) -> PathBuf {
        self.task_dir().join("workspace")
    }

    fn inspect_root(&self) -> PathBuf {
        self.path("inspect")
    }

    /// `<inspect_root>/<task>/*`.
    fn inspect_dirs(&self) -> Vec<PathBuf> {
        match fs::read_dir(self.inspect_root().join(self.task.as_str())) {
            Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn cgroup_root(&self) -> PathBuf {
        match &self.cfg.jail {
            JailMode::Jailed(jc) => jc.cgroup_root.clone(),
            JailMode::Unjailed => panic!("not jailed"),
        }
    }

    fn inspector(&self) -> Inspector {
        Inspector::new(self.cfg.clone(), self.inspect_root()).with_env(test_env())
    }

    fn base(&self) -> Digest {
        workspace_digest(&self.path("snapshot")).unwrap()
    }

    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        EffectRequest {
            effect_id: EffectId::derive(&self.task, 0, &kind, &Digest::of(payload)),
            task_id: self.task.clone(),
            kind,
            payload: payload.to_vec(),
            contract: self.contract.clone(),
            deadline_ts: 0,
        }
    }

    fn patch(&self, patch: &str) -> EffectRequest {
        self.request(EffectKind::ApplyPatch { expected_base: self.base() }, patch.as_bytes())
    }

    fn job(&self, req: &EffectRequest, ctx: &AttemptCtx) -> JobDir {
        let request = JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms: i64::MAX,
            task_deadline_ms: i64::MAX,
            worker: WorkerConfig::Firecracker(self.cfg.clone()),
        };
        JobDir::create(&self.path("jobs"), &request).unwrap().0
    }

    /// Runs `req` through a `FirecrackerWorker` (no supervisor).
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        let job = self.job(req, ctx);
        FirecrackerWorker::new(&self.cfg, &job).with_env(test_env()).run(req, ctx).await
    }

    async fn snapshot(&self) -> Digest {
        let out = self.run(&self.request(EffectKind::ReadSnapshot, b""), &ctx()).await;
        succeeded(&out);
        assert_eq!(out.new_workspace, Some(self.base()));
        self.base()
    }

    fn fake_guest_needle(&self) -> String {
        format!("fake-guest v.sock {}", self.task_dir().display())
    }
}

fn succeeded(out: &ExecOutcome) {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
}

/// Pids (other than ours) whose command line contains `needle` and that are not zombies.
fn pids_with(needle: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if pid == me || gone(pid) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
        if String::from_utf8_lossy(&cmdline).replace('\0', " ").contains(needle) {
            found.push(pid);
        }
    }
    found
}

/// Gone from `/proc`, or a zombie (nothing may reap orphans in the test container).
fn gone(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_whitespace().next() == Some("Z")
}

fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Takes `lock`, retrying while a descriptor of it lingers in a child another test thread
/// forked (closed at that child's `exec`).
fn lock_patiently(lock: &fs::File) {
    let until = Instant::now() + Duration::from_secs(2);
    while let Err(e) = lock.try_lock() {
        assert!(Instant::now() < until, "ws.lock stays busy: {e:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn patch_state(answer: Answer) -> (PatchStateKind, Option<Digest>, Vec<String>) {
    match answer {
        Answer::PatchState(s) => (s.state, s.workspace_digest, s.paths),
        Answer::Digest(d) => panic!("expected a patch state, got the digest {d}"),
    }
}

fn query_patch(fx: &Fx, inspector: &Inspector, patch: &str) -> Result<Answer, String> {
    inspector.query(&fx.task, Query::PatchState { expected_base: fx.base(), patch: patch.as_bytes().to_vec() })
}

// ---------------------------------------------------------------------------------------
// The reconcile trichotomy and current_workspace.

#[tokio::test]
async fn reconcile_trichotomy_on_base_patched_and_tampered_workspace() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let inspector = fx.inspector();
    let reconciler = Reconciler::Firecracker(fx.inspector());
    let req = fx.patch(&fix_patch());

    // The base: not applied.
    let (state, digest, _) = patch_state(query_patch(&fx, &inspector, &fix_patch()).unwrap());
    assert_eq!((state, digest), (PatchStateKind::NotApplied, Some(base)));
    assert_eq!(reconciler.reconcile(&req, &ctx()).await, Reconciliation::NotApplied);

    // The base plus the patch: applied, with exactly the bytes a live ApplyPatch gives (and
    // the host worker's).
    let live = fx.run(&req, &ctx()).await;
    succeeded(&live);
    let patched = live.new_workspace.unwrap();
    assert_ne!(patched, base);
    let (state, digest, paths) = patch_state(query_patch(&fx, &inspector, &fix_patch()).unwrap());
    assert_eq!((state, digest, paths), (PatchStateKind::Applied, Some(patched), vec!["src/parser.py".to_string()]));
    let c = ctx();
    let Reconciliation::Applied(out) = reconciler.reconcile(&req, &c).await else { panic!("not applied") };
    assert_eq!(out.output, live.output);
    assert_eq!(out.new_workspace, Some(patched));
    assert_eq!((out.receipt.attempt_id.clone(), out.receipt.lease_generation), (c.attempt_id.clone(), c.lease_generation));
    assert!(!out.unresolved);
    // The host worker over a copy of the same snapshot builds the same bytes.
    let host_dir = tempfile::tempdir().unwrap();
    let host = FixtureExecutor::new(fx.path("snapshot"), fx.path("profile"), host_dir.path().join("work"));
    succeeded(&host.run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx()).await);
    let host_out = host.run(&req, &ctx()).await;
    succeeded(&host_out);
    assert_eq!(out.output, host_out.output);

    // Neither: unknown, never "not applied".
    let parser = fx.guest_workspace().join("src/parser.py");
    let mut text = fs::read_to_string(&parser).unwrap();
    text.push_str("# tampered\n");
    fs::write(&parser, text).unwrap();
    let (state, digest, _) = patch_state(query_patch(&fx, &inspector, &fix_patch()).unwrap());
    assert_eq!(state, PatchStateKind::Unknown);
    assert_eq!(digest, Some(workspace_digest(&fx.guest_workspace()).unwrap()));
    assert_eq!(reconciler.reconcile(&req, &ctx()).await, Reconciliation::Unknown);

    // Only a patch is reconciled; other kinds are unknown without a boot.
    assert_eq!(reconciler.reconcile(&fx.request(EffectKind::RunVerification, b""), &ctx()).await, Reconciliation::Unknown);
}

#[tokio::test]
async fn current_workspace_reports_the_guest_digest_and_a_missing_image() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let reconciler = Reconciler::Firecracker(fx.inspector());
    assert_eq!(reconciler.current_workspace(&fx.task), Some(Ok(base)));
    match fx.inspector().query(&fx.task, Query::Digest).unwrap() {
        Answer::Digest(d) => assert_eq!(d, base),
        Answer::PatchState(s) => panic!("expected a digest, got {s:?}"),
    }

    fs::remove_file(fx.ws_img()).unwrap();
    let missing = format!("workspace image {} is missing", fx.ws_img().display());
    assert_eq!(reconciler.current_workspace(&fx.task), Some(Err(missing.clone())));
    assert_eq!(reconciler.reconcile(&fx.patch(&fix_patch()), &ctx()).await, Reconciliation::Unknown);
    assert!(fx.inspect_dirs().is_empty(), "nothing is booted for a missing image");
    // A task that never had a snapshot: the same answer.
    let other = TaskId::new();
    let err = reconciler.current_workspace(&other).unwrap().unwrap_err();
    assert_eq!(err, format!("workspace image {} is missing", fx.path("work").join(other.as_str()).join("ws.img").display()));
}

#[tokio::test]
async fn inspection_failure_is_unknown_never_not_applied() {
    let fx = Fx::new();
    fx.snapshot().await;
    let failing = || {
        Inspector::new(fx.cfg.clone(), fx.inspect_root())
            .with_env(env_with(&[(NEVER_LISTEN, "1")]))
            .with_boot_timeout(Duration::from_secs(1))
    };
    // The workspace is the base: a working inspector would say NotApplied.
    let started = Instant::now();
    assert_eq!(Reconciler::Firecracker(failing()).reconcile(&fx.patch(&fix_patch()), &ctx()).await, Reconciliation::Unknown);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let err = Reconciler::Firecracker(failing()).current_workspace(&fx.task).unwrap().unwrap_err();
    assert!(err.starts_with("workspace inspection failed: "), "{err}");
    assert!(err.contains("guest did not come up"), "{err}");

    // The failed inspections' directories are kept for diagnosis, with their logs.
    let dirs = fx.inspect_dirs();
    assert_eq!(dirs.len(), 1, "the second inspection collected the first one's directory: {dirs:?}");
    for name in ["console.log", "stderr.log", "firecracker.log", "vm.json"] {
        assert!(dirs[0].join(name).is_file(), "{name} is kept in {}", dirs[0].display());
    }
    assert!(!dirs[0].join("scratch.img").exists(), "the scratch image is not kept");
    assert!(pids_with(&fx.fake_guest_needle()).is_empty(), "no guest is left running");

    // A working inspector afterwards collects the dead directory and answers.
    assert_eq!(Reconciler::Firecracker(fx.inspector()).reconcile(&fx.patch(&fix_patch()), &ctx()).await, Reconciliation::NotApplied);
    assert!(fx.inspect_dirs().is_empty());
}

#[tokio::test]
async fn inspection_is_bounded_by_inspect_timeout() {
    assert_eq!(INSPECT_TIMEOUT, Duration::from_secs(60));
    let fx = Fx::new();
    fx.snapshot().await;
    let inspector = Inspector::new(fx.cfg.clone(), fx.inspect_root())
        .with_env(env_with(&[(HANG_INSPECT, "1")]))
        .with_inspect_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let err = query_patch(&fx, &inspector, &fix_patch()).unwrap_err();
    let took = started.elapsed();
    assert_eq!(err, "workspace inspection failed: timeout after 1s");
    assert!(took >= Duration::from_secs(1) && took < Duration::from_secs(2), "{took:?}");
    assert!(pids_with(&fx.fake_guest_needle()).is_empty(), "the hung guest is gone");
    // The digest query is not hooked: the same inspector answers it.
    assert!(matches!(inspector.query(&fx.task, Query::Digest), Ok(Answer::Digest(_))));
}

#[tokio::test]
async fn ws_lock_held_makes_inspection_unknown() {
    let fx = Fx::new();
    fx.snapshot().await;
    let lock = fs::File::options().create(true).truncate(false).write(true).open(fx.task_dir().join("ws.lock")).unwrap();
    lock_patiently(&lock);
    assert_eq!(fx.inspector().query(&fx.task, Query::Digest).unwrap_err(), "workspace image is attached to another VM");
    let reconciler = Reconciler::Firecracker(fx.inspector());
    assert_eq!(reconciler.reconcile(&fx.patch(&fix_patch()), &ctx()).await, Reconciliation::Unknown);
    assert_eq!(reconciler.current_workspace(&fx.task), Some(Err("workspace image is attached to another VM".into())));
    assert!(fx.inspect_dirs().is_empty(), "nothing is booted while the image is attached");
    drop(lock);
    assert_eq!(reconciler.current_workspace(&fx.task), Some(Ok(fx.base())));
}

#[tokio::test]
async fn a_successful_inspection_removes_its_directory() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    assert!(matches!(fx.inspector().query(&fx.task, Query::Digest), Ok(Answer::Digest(d)) if d == base));
    assert!(fx.inspect_root().join(fx.task.as_str()).is_dir());
    assert!(fx.inspect_dirs().is_empty(), "{:?}", fx.inspect_dirs());
    assert!(pids_with(&fx.fake_guest_needle()).is_empty());
    // The lock is free again.
    let lock = fs::File::options().write(true).open(fx.task_dir().join("ws.lock")).unwrap();
    lock_patiently(&lock);
}

#[tokio::test]
async fn the_fake_launcher_needs_the_test_switch() {
    let fx = Fx::new();
    fx.snapshot().await;
    // No AGENTOS_TEST_WORKERS=1 in this inspector's environment (nor in the test process's).
    let err = Inspector::new(fx.cfg.clone(), fx.inspect_root()).query(&fx.task, Query::Digest).unwrap_err();
    assert!(err.starts_with("workspace inspection failed: firecracker worker unavailable: "), "{err}");
    assert!(fx.inspect_dirs().is_empty());
}

// ---------------------------------------------------------------------------------------
// Through the supervisor: attempt tokens and the kill-after-request reconciliation.

fn request_token(job: &JobDir) -> String {
    match job.request().unwrap().worker {
        WorkerConfig::Firecracker(cfg) => cfg.attempt_token,
        other => panic!("not a Firecracker job: {other:?}"),
    }
}

/// Every file under `dir` (recursively), with its bytes.
fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            out.extend(files_under(&path));
        } else if ty.is_file() {
            out.push((path.clone(), fs::read(&path).unwrap()));
        }
    }
    out
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn attempt_token_is_minted_fresh_per_job_and_never_logged() {
    let fx = Fx::new();
    let counts = ExecCounts::default();
    let exec = supervised(&fx.path("jobs"), WorkerConfig::Firecracker(fx.cfg.clone()), &counts, None, &[(TEST_WORKERS, "1")]);
    let first = fx.request(EffectKind::ReadSnapshot, b"");
    succeeded(&exec.run(&first, &ctx()).await);
    let second = fx.request(EffectKind::RunVerification, b"");
    succeeded(&exec.run(&second, &ctx()).await);

    let jobs: Vec<JobDir> =
        [&first, &second].iter().map(|r| JobDir::list(&fx.path("jobs"), &r.effect_id).unwrap().remove(0)).collect();
    let tokens: Vec<String> = jobs.iter().map(request_token).collect();
    for t in &tokens {
        assert!(is_attempt_token(t), "{t:?}");
        assert_ne!(*t, fx.cfg.attempt_token, "the executor's own config token is never used");
    }
    assert_ne!(tokens[0], tokens[1], "a fresh token per job");

    for job in &jobs {
        for name in ["supervisor.log", "console.log", "vm.json", "request.json"] {
            assert!(job.path.join(name).is_file(), "{name} exists in {}", job.path.display());
        }
        let files = files_under(&job.path);
        assert!(files.len() >= 4, "{files:?}");
        for (path, bytes) in files {
            if path.file_name().unwrap() == "request.json" {
                continue;
            }
            for t in &tokens {
                assert!(!contains(&bytes, t), "token in {}", path.display());
            }
        }
    }
}

#[tokio::test]
async fn patch_killed_after_request_is_reconciled_by_inspection_not_failed() {
    let fx = Fx::new();
    let counts = ExecCounts::default();
    let exec = supervised(
        &fx.path("jobs"),
        WorkerConfig::Firecracker(fx.cfg.clone()),
        &counts,
        None,
        &[(TEST_WORKERS, "1"), (KILL_VM_AFTER_REQUEST, "1")],
    );
    succeeded(&exec.run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx()).await);
    let (req, c) = (fx.patch(&fix_patch()), ctx());
    let out = exec.run(&req, &c).await;
    assert!(!out.unresolved, "never unresolved: {}", String::from_utf8_lossy(&out.output));
    assert_eq!((out.receipt.attempt_id.clone(), out.receipt.effect_id.clone()), (c.attempt_id.clone(), req.effect_id.clone()));
    // The worker wrote no outcome and the supervisor no receipt: this came from inspection.
    let job = JobDir::list(&fx.path("jobs"), &req.effect_id).unwrap().remove(0);
    assert_eq!(job.read_receipt(), None);
    assert!(job.read_outcome().is_none(), "the worker wrote no outcome");
    let inspected = match fx.inspector().query(&fx.task, Query::Digest).unwrap() {
        Answer::Digest(d) => d,
        Answer::PatchState(s) => panic!("{s:?}"),
    };
    match &out.receipt.outcome {
        Outcome::Success => {
            assert_eq!(out.new_workspace, Some(inspected));
            assert_ne!(inspected, fx.base());
        }
        Outcome::Failure(reason) => {
            assert_eq!(reason, "patch provably not applied");
            assert_eq!(inspected, fx.base());
        }
    }
    assert!(fx.inspect_dirs().is_empty(), "the reconciling inspection cleaned up after itself");
}

// ---------------------------------------------------------------------------------------
// The inspector's VM dies with the controller.

const CHILD_CFG: &str = "AGENTOS_TEST_INSPECTOR_CHILD_CFG";
const CHILD_ROOT: &str = "AGENTOS_TEST_INSPECTOR_CHILD_ROOT";
const CHILD_TASK: &str = "AGENTOS_TEST_INSPECTOR_CHILD_TASK";
const CHILD_BASE: &str = "AGENTOS_TEST_INSPECTOR_CHILD_BASE";

/// Helper run by `inspector_dies_with_the_controller` in a child process (the controller):
/// one inspection whose guest hangs. Never run on its own.
#[test]
#[ignore = "helper process for inspector_dies_with_the_controller"]
fn inspector_child_process() {
    let (Ok(cfg), Ok(root), Ok(task), Ok(base)) =
        (std::env::var(CHILD_CFG), std::env::var(CHILD_ROOT), std::env::var(CHILD_TASK), std::env::var(CHILD_BASE))
    else {
        return;
    };
    let cfg: FirecrackerConfig = serde_json::from_str(&cfg).unwrap();
    let task: TaskId = serde_json::from_str(&format!("\"{task}\"")).unwrap();
    let base: Digest = serde_json::from_str(&format!("\"{base}\"")).unwrap();
    let inspector = Inspector::new(cfg, PathBuf::from(root)).with_env(env_with(&[(HANG_INSPECT, "1")]));
    let _ = inspector.query(&task, Query::PatchState { expected_base: base, patch: fix_patch().into_bytes() });
}

#[tokio::test]
async fn inspector_dies_with_the_controller() {
    let fx = Fx::new();
    fx.snapshot().await;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "inspector_child_process", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_CFG, serde_json::to_string(&fx.cfg).unwrap())
        .env(CHILD_ROOT, fx.inspect_root())
        .env(CHILD_TASK, fx.task.as_str())
        .env(CHILD_BASE, fx.base().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Mid-inspection: the guest is up and bound its socket in the inspect directory.
    let needle = fx.fake_guest_needle();
    wait_until("the inspector's guest", PATIENCE, || {
        !pids_with(&needle).is_empty() && fx.inspect_dirs().iter().any(|d| d.join("v.sock").exists())
    });
    let guest = pids_with(&needle)[0];
    thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "the inspection is still under way");
    assert!(!gone(guest));

    let _ = kill_process(Pid::from_raw(child.id() as i32).unwrap(), Signal::KILL);
    child.wait().unwrap();
    let killed = Instant::now();
    wait_until("the guest to exit on EOF", Duration::from_secs(2), || gone(guest));
    assert!(killed.elapsed() < Duration::from_millis(500), "{:?}", killed.elapsed());

    // The next inspection collects the dead inspector's directory and answers.
    assert!(matches!(fx.inspector().query(&fx.task, Query::Digest), Ok(Answer::Digest(d)) if d == fx.base()));
    assert!(fx.inspect_dirs().is_empty());
}

// ---------------------------------------------------------------------------------------
// Jailed inspections through the fake jailer.

#[tokio::test]
async fn jailed_inspector_boots_through_the_jailer_with_id_inspect_uuid() {
    let fx = Fx::jailed();
    fx.snapshot().await;
    // Held up by the hang hook, so the jail is there to look at.
    let held = Inspector::new(fx.cfg.clone(), fx.inspect_root())
        .with_env(env_with(&[(HANG_INSPECT, "1")]))
        .with_inspect_timeout(Duration::from_secs(3));
    let (task, base) = (fx.task.clone(), fx.base());
    let running = thread::spawn(move || held.query(&task, Query::PatchState { expected_base: base, patch: fix_patch().into_bytes() }));
    let mut argv = String::new();
    let mut dir = PathBuf::new();
    wait_until("the jailer's argv", PATIENCE, || {
        for d in fx.inspect_dirs() {
            if let Ok(text) = fs::read_to_string(d.join("jail/argv.txt")) {
                (argv, dir) = (text, d);
                return true;
            }
        }
        false
    });
    let uuid = dir.file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(uuid.len(), 36, "{uuid}");
    assert!(argv.contains(&format!("--id inspect-{uuid} ")), "{argv}");
    assert!(argv.contains(&format!("--chroot-base-dir {} ", dir.join("jail").display())), "{argv}");
    assert!(argv.contains("-- --no-api --config-file /vm.json"), "{argv}");
    assert!(fx.cgroup_root().join("agentos").join(format!("inspect-{uuid}")).is_dir());
    let chroot = dir.join("jail/firecracker").join(format!("inspect-{uuid}")).join("root");
    assert_eq!(fs::metadata(chroot.join("ws.img")).unwrap().ino(), fs::metadata(fx.ws_img()).unwrap().ino());
    assert!(!dir.join("vm.json").exists(), "jailed, vm.json is in the chroot only");
    let err = running.join().unwrap().unwrap_err();
    assert!(err.contains("timeout after 3s"), "{err}");
    assert!(!dir.join("jail").exists(), "the jail is collected after a failure too");
    assert!(!fx.cgroup_root().join("agentos").join(format!("inspect-{uuid}")).exists());

    // The answer equals the unjailed inspector's over the same image.
    let jailed = fx.inspector().query(&fx.task, Query::Digest).unwrap();
    let mut unjailed_cfg = fx.cfg.clone();
    unjailed_cfg.jail = JailMode::Unjailed;
    let unjailed = Inspector::new(unjailed_cfg, fx.path("inspect-unjailed")).with_env(test_env()).query(&fx.task, Query::Digest).unwrap();
    assert_eq!(jailed, unjailed);
    assert_eq!(jailed, Answer::Digest(fx.base()));
    let jailed = patch_state(query_patch(&fx, &fx.inspector(), &fix_patch()).unwrap());
    assert_eq!(jailed.0, PatchStateKind::NotApplied);
    assert!(fx.inspect_dirs().is_empty());
}

/// A dead inspector's directory: the staged jail's tree, its marker and its (empty) cgroup.
fn plant_dead_inspection(fx: &Fx) -> (PathBuf, PathBuf) {
    let uuid = AttemptId::new().to_string();
    let dir = fx.inspect_root().join(fx.task.as_str()).join(&uuid);
    let id = format!("inspect-{uuid}");
    let root = dir.join("jail/firecracker").join(&id).join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("vm.json"), b"{}").unwrap();
    fs::write(dir.join("console.log"), b"").unwrap();
    let cgroup = fx.cgroup_root().join("agentos").join(&id);
    fs::create_dir_all(&cgroup).unwrap();
    fs::write(dir.join("jail/cgroup"), format!("{}\n", cgroup.display())).unwrap();
    (dir, cgroup)
}

#[tokio::test]
async fn dead_inspect_directories_are_collected_before_a_new_inspection() {
    let fx = Fx::jailed();
    fx.snapshot().await;
    let dead = [plant_dead_inspection(&fx), plant_dead_inspection(&fx)];
    // Another task's inspect directory is not this inspection's business.
    let other = fx.inspect_root().join(TaskId::new().as_str()).join(AttemptId::new().to_string());
    fs::create_dir_all(&other).unwrap();

    assert_eq!(fx.inspector().query(&fx.task, Query::Digest).unwrap(), Answer::Digest(fx.base()));
    for (dir, cgroup) in &dead {
        assert!(!dir.exists(), "{} is collected", dir.display());
        assert!(!cgroup.exists(), "{} is removed", cgroup.display());
    }
    assert!(fx.inspect_dirs().is_empty());
    assert!(other.is_dir());
}

#[tokio::test]
async fn a_failed_jailed_inspection_keeps_its_directory_but_not_its_jail() {
    let fx = Fx::jailed();
    fx.snapshot().await;
    let err = Inspector::new(fx.cfg.clone(), fx.inspect_root())
        .with_env(env_with(&[(NEVER_LISTEN, "1")]))
        .with_boot_timeout(Duration::from_secs(1))
        .query(&fx.task, Query::Digest)
        .unwrap_err();
    assert!(err.starts_with("workspace inspection failed: guest did not come up"), "{err}");
    let dirs = fx.inspect_dirs();
    assert_eq!(dirs.len(), 1);
    for name in ["console.log", "stderr.log", "firecracker.log"] {
        assert!(dirs[0].join(name).is_file(), "{name} is kept");
    }
    assert!(!dirs[0].join("jail").exists(), "the jail is collected");
    let cgroups: Vec<_> = fs::read_dir(fx.cgroup_root().join("agentos")).unwrap().collect();
    assert!(cgroups.is_empty(), "the cgroup is removed: {cgroups:?}");
    assert!(pids_with(&fx.fake_guest_needle()).is_empty());
}

#[tokio::test]
async fn controller_collects_a_dead_jobs_jail_only_after_settlement() {
    let fx = Fx::jailed();
    let counts = ExecCounts::default();
    let worker = WorkerConfig::Firecracker(fx.cfg.clone());
    let plain = supervised(&fx.path("jobs"), worker.clone(), &counts, None, &[(TEST_WORKERS, "1")]);
    succeeded(&plain.run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx()).await);
    // A verification that hangs (bounded, so a failure cannot wedge the suite).
    let marker = format!("agentos-inspector-hold-{}", fx.task);
    let profile = serde_json::json!({
        "id": "hold",
        "command": ["python3", "-c", "import time\ntime.sleep(30)", marker],
        "protected": true,
    });
    fs::write(fx.path("profile/profile.json"), profile.to_string()).unwrap();
    let hook = CrashHook::at(CrashPoint::DuringExecute, "run_verification");
    let crashing = supervised(&fx.path("jobs"), worker, &counts, Some(hook), &[(TEST_WORKERS, "1")]);
    let req = fx.request(EffectKind::RunVerification, b"");
    // The controller "dies" right after the launch: nothing settles or collects the job.
    crashing.run(&req, &ctx()).await;
    let job = JobDir::list(&fx.path("jobs"), &req.effect_id).unwrap().remove(0);
    wait_until("the check to run", PATIENCE, || !pids_with(&marker).is_empty());
    let marker_text = fs::read_to_string(job.path.join("jail/cgroup")).unwrap();
    let cgroup = PathBuf::from(marker_text.trim_end());
    assert!(job.path.join("jail").is_dir() && cgroup.is_dir(), "the jail is there while the job runs");
    assert!(!job.is_dead());

    // The supervisor dies: the job is dead by its lock, but its worker, VM and check live on.
    let supervisor = job.read_status().unwrap().supervisor_pid.unwrap() as i32;
    let _ = kill_process(Pid::from_raw(supervisor).unwrap(), Signal::KILL);
    wait_until("the lock to come free", PATIENCE, || job.is_dead());
    assert!(job.path.join("jail").is_dir(), "nothing collects a dead job before it is settled");

    let recovering = supervised(&fx.path("jobs"), WorkerConfig::Firecracker(fx.cfg.clone()), &counts, None, &[(TEST_WORKERS, "1")]);
    assert!(recovering.fence_jobs(std::slice::from_ref(&job)).await, "the fence settles the job");
    assert!(pids_with(&marker).is_empty(), "the check is gone");
    assert!(!job.path.join("jail").exists(), "the jail is collected once the job is settled");
    assert!(!cgroup.exists(), "the cgroup is removed");
    // The receipt handling is 3a's: no receipt, so recovery would reconcile or retry.
    assert_eq!(job.read_receipt(), None);
}
