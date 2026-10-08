//! The KVM tier: the real guest image booted by the real Firecracker under the real jailer.
//! Every test of the tier begins with `let Some(kvm) = kvm::require() else { return };`
//! (`docker compose run --rm test-kvm …`; without `AGENTOS_KVM_TESTS` each prints SKIPPED).
//! The file starts with the gate's own tests, which run in every tier: the gate is exercised
//! in child processes of this very test binary (`--exact` on the entry below), so each child
//! gets exactly the environment the test gives it.
//!
//! What the fakes cannot show is proven here: the guest's boot path (mounts, mkfs, vsock,
//! uid drop, read-only root), the threat model's hostile profiles (network, secrets, CPU,
//! memory, processes, disk), the jail (uid, chroot, cgroup limits enforced), and the crash
//! and kill paths with a real VM.

mod common;

use common::kvm;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Set only in the children: makes `gate_child_entry` call `require()`.
const CHILD: &str = "AGENTOS_KVM_GATE_CHILD";
const GATE_VARS: [&str; 4] = [
    "AGENTOS_KVM_TESTS",
    "AGENTOS_FIRECRACKER",
    "AGENTOS_JAILER",
    "AGENTOS_GUEST_IMAGE",
];

/// Not a test of its own: in a child it reports what `require()` returned.
#[test]
fn gate_child_entry() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let got = kvm::require();
    println!(
        "GATE_RESULT: {}",
        if got.is_some() {
            "available"
        } else {
            "skipped"
        }
    );
}

fn gate_child(env: &[(&str, &Path)], kvm_tests: bool) -> Output {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "gate_child_entry",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD, "1");
    for var in GATE_VARS {
        cmd.env_remove(var);
    }
    if kvm_tests {
        cmd.env("AGENTOS_KVM_TESTS", "1");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// Stand-ins that pass every check but `/dev/kvm` and the jail's environment: a
/// `firecracker` and a `jailer` that print their v1.17 versions, and an image directory
/// with the three files.
struct StandIns {
    _dir: tempfile::TempDir,
    firecracker: PathBuf,
    jailer: PathBuf,
    image: PathBuf,
}

fn stand_ins() -> StandIns {
    let dir = tempfile::tempdir().unwrap();
    let firecracker = script(
        &dir.path().join("firecracker"),
        "echo 'Firecracker v1.17.0'",
    );
    let jailer = script(&dir.path().join("jailer"), "echo 'Jailer v1.17.0'");
    let image = dir.path().join("image");
    fs::create_dir_all(&image).unwrap();
    for f in ["image.json", "vmlinux", "rootfs.squashfs"] {
        fs::write(image.join(f), b"x").unwrap();
    }
    StandIns {
        _dir: dir,
        firecracker,
        jailer,
        image,
    }
}

#[test]
fn kvm_gate_skips_loudly_without_the_variable() {
    let out = gate_child(&[], false);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout.contains("SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)"),
        "{stdout}"
    );
    assert!(stdout.contains("GATE_RESULT: skipped"), "{stdout}");
}

#[test]
fn kvm_gate_panics_when_requested_but_unavailable() {
    let s = stand_ins();
    let missing = Path::new("/nonexistent");

    let out = gate_child(
        &[
            ("AGENTOS_FIRECRACKER", missing),
            ("AGENTOS_JAILER", &s.jailer),
            ("AGENTOS_GUEST_IMAGE", &s.image),
        ],
        true,
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr.contains("AGENTOS_FIRECRACKER=/nonexistent"),
        "{stderr}"
    );
    assert!(!stderr.contains("AGENTOS_JAILER="), "{stderr}");
    assert!(
        !text(&out.stdout).contains("GATE_RESULT"),
        "the gate returned instead of panicking"
    );

    let out = gate_child(
        &[
            ("AGENTOS_FIRECRACKER", &s.firecracker),
            ("AGENTOS_JAILER", missing),
            ("AGENTOS_GUEST_IMAGE", &s.image),
        ],
        true,
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains("AGENTOS_JAILER=/nonexistent"), "{stderr}");
    assert!(!stderr.contains("AGENTOS_FIRECRACKER="), "{stderr}");

    let out = gate_child(
        &[
            ("AGENTOS_FIRECRACKER", &s.firecracker),
            ("AGENTOS_JAILER", &s.jailer),
            ("AGENTOS_GUEST_IMAGE", missing),
        ],
        true,
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr.contains("AGENTOS_GUEST_IMAGE=/nonexistent"),
        "{stderr}"
    );
    assert!(stderr.contains("image.json"), "{stderr}");

    // Everything else in place: the jail's own environment still decides. The `test`
    // service is root with a read-only cgroup tree, so the gate cannot pass without the
    // jail; a non-root run fails earlier on root.
    let mounts = fs::read_to_string("/proc/mounts").unwrap();
    let cgroup_ro = mounts.lines().any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 4 && f[2] == "cgroup2" && f[3].split(',').any(|o| o == "ro")
    });
    let root = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .is_some_and(|l| l.split_whitespace().nth(2) == Some("0"));
    let expected = match (root, cgroup_ro) {
        (false, _) => "needs root",
        (true, true) => "read-only",
        (true, false) => {
            println!(
                "cgroup tree is writable here (test-kvm): the jail-environment case belongs to the `test` service"
            );
            return;
        }
    };
    let out = gate_child(
        &[
            ("AGENTOS_FIRECRACKER", &s.firecracker),
            ("AGENTOS_JAILER", &s.jailer),
            ("AGENTOS_GUEST_IMAGE", &s.image),
        ],
        true,
    );
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains(expected), "{stderr}");
    assert!(stderr.contains("jail probe"), "{stderr}");
}

// ---------------------------------------------------------------------------------------
// The tier's fixture and probes.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::guest::{Message, Mode, SCRATCH_IMAGE_BYTES, WS_IMAGE_BYTES, mint_attempt_token};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::resources::VmResources;
use agentos_core::state::TaskState;
use agentos_engine::agent::FakeAgent;
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::firecracker::{
    Answer, FirecrackerConfig, FirecrackerWorker, Inspector, Query, render_vm_json,
};
use agentos_engine::guestlink::GuestLink;
use agentos_engine::jail::{self, JAIL_UID, JailMode, StageSources};
use agentos_engine::job::{JobDir, JobRequest, JobState, KillReason, WorkerConfig};
use agentos_engine::recover::{Decision, recover};
use agentos_engine::runner::{EngineError, run_task, run_task_with};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_engine::worker::Worker;
use agentos_engine::workspace::workspace_digest;
use common::{
    Env, FcProc, SUPERVISOR_BIN, TEST_WORKERS_ENV, contract, copy_dir, firecracker_processes,
    fix_patch, fixtures, home_firecrackers, processes_naming, supervised,
};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use tempfile::TempDir;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Upper bound for anything a test waits on outside the engine.
const PATIENCE: Duration = Duration::from_secs(60);
/// Linux's `USER_HZ`: the unit of `utime`/`stime` in `/proc/<pid>/stat` (100 on x86-64).
const CLOCK_TICKS: u64 = 100;
/// The jail's `pids.max` and the bound `fork_bomb_never_adds_a_host_process` holds Firecracker
/// to: its main thread, one per vCPU, and a few of its own.
const MAX_FIRECRACKER_TASKS: u64 = 1 + 1 + 4;

/// Tests that measure host-wide state (the listening sockets of the network namespace) or
/// must not compete for the host's I/O take this exclusively; every other VM test shares
/// it, so none of them runs meanwhile. (Each test has its own runtime: tokio's lock works
/// across them and may be held across `.await`.)
static HOST_WIDE: RwLock<()> = RwLock::const_new(());

async fn shared() -> RwLockReadGuard<'static, ()> {
    HOST_WIDE.read().await
}

async fn exclusive() -> RwLockWriteGuard<'static, ()> {
    HOST_WIDE.write().await
}

fn test_env() -> Vec<(String, String)> {
    vec![(TEST_WORKERS_ENV.into(), "1".into())]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn out_text(out: &ExecOutcome) -> String {
    String::from_utf8_lossy(&out.output).into_owned()
}

fn failure(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!("expected a failure, got success: {}", out_text(out)),
    }
}

fn evidence(out: &ExecOutcome) -> serde_json::Value {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", out_text(out));
    serde_json::from_slice(&out.output).unwrap()
}

/// The check's one JSON line of findings, from the evidence's `stdout`.
fn findings(evidence: &serde_json::Value) -> serde_json::Value {
    let stdout = evidence["stdout"].as_str().unwrap();
    let line = stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("no findings line in {stdout:?}"));
    serde_json::from_str(line).unwrap()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

async fn wait_for_async(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn alive(pid: i32) -> bool {
    common::proc_state(pid).is_some_and(|(s, ..)| s != "Z")
}

/// `VmRSS` of `pid` in KiB.
fn rss_kib(pid: i32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `utime + stime` of `pid` (every thread), in clock ticks.
fn cpu_ticks(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 2..].split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

/// `key value` lines of a cgroup file (`memory.events`, `cpu.stat`).
fn keyed(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            l.split_once(' ')
                .and_then(|(k, v)| Some((k.to_string(), v.trim().parse().ok()?)))
        })
        .collect()
}

/// Runs `probe` every `every` on a thread until `stop`, keeping what it returns.
struct Sampler<T> {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<Vec<T>>,
}

fn sample<T: Send + 'static>(
    every: Duration,
    mut probe: impl FnMut(Duration) -> Option<T> + Send + 'static,
) -> Sampler<T> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let handle = thread::spawn(move || {
        let started = Instant::now();
        let mut kept = Vec::new();
        while !flag.load(Ordering::Relaxed) {
            if let Some(v) = probe(started.elapsed()) {
                kept.push(v);
            }
            thread::sleep(every);
        }
        kept
    });
    Sampler { stop, handle }
}

impl<T> Sampler<T> {
    fn stop(self) -> Vec<T> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap()
    }
}

/// What one sample saw of a VM: its process and its cgroup's files.
#[derive(Debug, Clone)]
struct VmSample {
    at: Duration,
    procs: Vec<FcProc>,
    rss_kib: Option<u64>,
    cpu_ticks: Option<u64>,
    cgroup: BTreeMap<&'static str, String>,
}

const CGROUP_FILES: [&str; 10] = [
    "cpu.max",
    "memory.max",
    "memory.swap.max",
    "pids.max",
    "pids.current",
    "memory.events",
    "memory.stat",
    "memory.current",
    "cpu.stat",
    "cgroup.procs",
];

/// Samples the Firecracker of VM `id` (and its cgroup under `cgroup_root`) every `every`.
fn watch_vm(id: String, cgroup_root: PathBuf, every: Duration) -> Sampler<VmSample> {
    let cgroup = cgroup_root.join("agentos").join(&id);
    sample(every, move |at| {
        let procs: Vec<FcProc> = firecracker_processes()
            .into_iter()
            .filter(|p| p.id() == Some(id.as_str()))
            .collect();
        let files: BTreeMap<&'static str, String> = CGROUP_FILES
            .iter()
            .filter_map(|f| Some((*f, fs::read_to_string(cgroup.join(f)).ok()?)))
            .collect();
        if procs.is_empty() && files.is_empty() {
            return None;
        }
        let pid = procs.first().map(|p| p.pid);
        Some(VmSample {
            at,
            rss_kib: pid.and_then(rss_kib),
            cpu_ticks: pid.and_then(cpu_ticks),
            procs,
            cgroup: files,
        })
    })
}

/// Cleans a test's home up however the test ends, a failed assertion included, so no VM
/// or `agentos/<id>` cgroup leaks into later tests: SIGKILLs every Firecracker of the home
/// (by `--id`: `home_vm_ids`, plus the ids registered with `watch`), waits for them to be
/// gone, then collects the jail of every job and inspect directory (and the watched ones).
/// Declare it after the home's `TempDir` (or first in a struct), so it runs before the
/// directory is removed.
struct HomeGuard {
    root: PathBuf,
    cgroup_root: PathBuf,
    watched: std::sync::Mutex<Vec<(String, PathBuf)>>,
}

impl HomeGuard {
    fn new(root: &Path, cgroup_root: &Path) -> HomeGuard {
        HomeGuard {
            root: root.to_path_buf(),
            cgroup_root: cgroup_root.to_path_buf(),
            watched: Default::default(),
        }
    }

    /// Also covers the VM `id` run from `dir` (a VM launched by hand, outside `jobs/`).
    fn watch(&self, id: &str, dir: &Path) {
        self.watched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((id.to_string(), dir.to_path_buf()));
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        let watched = std::mem::take(&mut *self.watched.lock().unwrap_or_else(|p| p.into_inner()));
        let mut ids = common::home_vm_ids(&self.root);
        ids.extend(watched.iter().map(|(id, _)| id.clone()));
        let ours = || -> Vec<i32> {
            firecracker_processes()
                .into_iter()
                .filter(|p| p.id().is_some_and(|id| ids.iter().any(|i| i == id)))
                .map(|p| p.pid)
                .collect()
        };
        for pid in ours() {
            if let Some(pid) = Pid::from_raw(pid) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !ours().is_empty() && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        let listed = |dir: PathBuf| -> Vec<PathBuf> {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .collect()
        };
        let mut dirs = listed(self.root.join("jobs"));
        for task in listed(self.root.join("inspect")) {
            dirs.extend(listed(task));
        }
        dirs.extend(watched.into_iter().map(|(_, dir)| dir));
        for dir in dirs {
            // A killed VM's cgroup may need a moment to empty.
            let until = Instant::now() + Duration::from_secs(2);
            while jail::collect(&dir, &self.cgroup_root).is_err() && Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A task of the real, jailed worker whose files live in a scratch root on the guest image's
/// filesystem (the jail hard-links the image).
struct Fx {
    /// First: dropped (VMs killed, jails collected) before `dir` is removed.
    guard: HomeGuard,
    kvm: kvm::Kvm,
    dir: TempDir,
    task: TaskId,
    contract: Contract,
    cfg: FirecrackerConfig,
    counts: ExecCounts,
    step: AtomicU32,
}

impl Fx {
    fn new(kvm: &kvm::Kvm) -> Fx {
        Fx::new_in(kvm, kvm.root())
    }

    /// As `new`, with everything (work root, jobs, the image copy) under `dir`.
    fn new_in(kvm: &kvm::Kvm, dir: TempDir) -> Fx {
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let cfg = kvm.jailed_config(dir.path());
        let guard = HomeGuard::new(dir.path(), &kvm.cgroup_root);
        Fx {
            guard,
            kvm: kvm.clone(),
            dir,
            task: TaskId::new(),
            contract: contract(10).0,
            cfg,
            counts: ExecCounts::default(),
            step: AtomicU32::new(0),
        }
    }

    /// The task's drive sizes and rate limits, as a recorded contract would set them.
    fn with_resources(mut self, resources: VmResources) -> Fx {
        self.cfg.resources = resources;
        self
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn task_dir(&self) -> PathBuf {
        self.path("work").join(self.task.as_str())
    }

    fn ws_img(&self) -> PathBuf {
        self.task_dir().join("ws.img")
    }

    /// Replaces the task's verification profile with `fixtures/profiles/<name>`.
    fn use_profile(&self, name: &str) {
        let profile = self.path("profile");
        fs::remove_dir_all(&profile).unwrap();
        copy_dir(&fixtures().join("profiles").join(name), &profile);
    }

    /// A one-line check (`python3 -c script`) as the task's profile.
    fn use_script(&self, script: &str) {
        let profile = serde_json::json!({ "id": "kvm", "command": ["python3", "-c", script], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    /// A request with an effect id of its own (each call is a new step).
    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        let step = self.step.fetch_add(1, Ordering::Relaxed);
        EffectRequest {
            effect_id: EffectId::derive(&self.task, step, &kind, &Digest::of(payload)),
            task_id: self.task.clone(),
            kind,
            payload: payload.to_vec(),
            contract: self.contract.clone(),
            deadline_ts: 0,
        }
    }

    fn job_request(
        &self,
        req: &EffectRequest,
        ctx: &AttemptCtx,
        cfg: &FirecrackerConfig,
        lease_in_ms: Option<i64>,
    ) -> JobRequest {
        JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms: lease_in_ms.map_or(i64::MAX, |l| now_ms() + l),
            task_deadline_ms: 0,
            worker: WorkerConfig::Firecracker(cfg.clone()),
        }
    }

    /// Runs `req` with the worker in this process (no supervisor), configured by `tune`.
    async fn run_with(
        &self,
        req: &EffectRequest,
        cfg: &FirecrackerConfig,
        tune: impl FnOnce(FirecrackerWorker) -> FirecrackerWorker,
    ) -> Ran {
        let ctx = ctx(self.step.load(Ordering::Relaxed) as u64 + 1);
        let job = JobDir::create(&self.path("jobs"), &self.job_request(req, &ctx, cfg, None))
            .unwrap()
            .0;
        let worker = tune(FirecrackerWorker::new(cfg, &job).with_env(test_env()));
        let started = Instant::now();
        let out = worker.run(req, &ctx).await;
        Ran {
            out,
            job,
            ctx,
            took: started.elapsed(),
        }
    }

    async fn run(&self, kind: EffectKind) -> Ran {
        let req = self.request(kind, b"");
        self.run_with(&req, &self.cfg.clone(), |w| w).await
    }

    /// `ReadSnapshot`, which must succeed.
    async fn snapshot(&self) -> Ran {
        let ran = self.run(EffectKind::ReadSnapshot).await;
        assert_eq!(
            ran.out.receipt.outcome,
            Outcome::Success,
            "{}",
            out_text(&ran.out)
        );
        ran
    }

    /// `RunVerification` with the worker tuned by `tune`, sampling its VM every `every`.
    async fn verify_watched(
        &self,
        every: Duration,
        tune: impl FnOnce(FirecrackerWorker) -> FirecrackerWorker,
    ) -> (Ran, Vec<VmSample>) {
        let req = self.request(EffectKind::RunVerification, b"");
        let ctx = ctx(self.step.load(Ordering::Relaxed) as u64 + 1);
        let job = JobDir::create(
            &self.path("jobs"),
            &self.job_request(&req, &ctx, &self.cfg, None),
        )
        .unwrap()
        .0;
        let worker = tune(FirecrackerWorker::new(&self.cfg, &job).with_env(test_env()));
        let watch = watch_vm(
            ctx.attempt_id.to_string(),
            self.kvm.cgroup_root.clone(),
            every,
        );
        let started = Instant::now();
        let out = worker.run(&req, &ctx).await;
        let took = started.elapsed();
        (
            Ran {
                out,
                job,
                ctx,
                took,
            },
            watch.stop(),
        )
    }

    fn inspector(&self) -> Inspector {
        Inspector::new(self.cfg.clone(), self.path("inspect")).with_env(test_env())
    }

    fn digest(&self) -> Digest {
        match self.inspector().query(&self.task, Query::Digest).unwrap() {
            Answer::Digest(d) => d,
            other => panic!("{other:?}"),
        }
    }

    /// A supervised executor (the real `agentos-supervisor`) over this task.
    fn executor(&self, crash: Option<CrashHook>, env: &[(&str, &str)]) -> SupervisedExecutor {
        supervised(
            &self.path("jobs"),
            WorkerConfig::Firecracker(self.cfg.clone()),
            &self.counts,
            crash,
            env,
        )
    }

    /// No Firecracker of this home is alive within `within`.
    fn assert_no_vm_within(&self, within: Duration) {
        let started = Instant::now();
        loop {
            let live = home_firecrackers(self.root());
            if live.is_empty() {
                return;
            }
            assert!(
                started.elapsed() < within,
                "Firecracker processes outlived the job: {live:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// No process of this home (supervisors, workers, VMs) is alive within `within`.
    fn assert_nothing_left_within(&self, within: Duration) {
        let started = Instant::now();
        loop {
            let live = processes_naming(self.root());
            let vms = home_firecrackers(self.root());
            if live.is_empty() && vms.is_empty() {
                return;
            }
            assert!(
                started.elapsed() < within,
                "processes outlived the job: {live:?} {vms:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn cgroup_of(&self, ctx: &AttemptCtx) -> PathBuf {
        self.kvm
            .cgroup_root
            .join("agentos")
            .join(ctx.attempt_id.to_string())
    }
}

/// One finished job of the worker run in this process.
struct Ran {
    out: ExecOutcome,
    job: JobDir,
    ctx: AttemptCtx,
    took: Duration,
}

impl Ran {
    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.job.path.join(name)).unwrap_or_default()
    }
}

fn ctx(lease: u64) -> AttemptCtx {
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: lease,
        worker: "kvm".into(),
    }
}

fn nlink(path: &Path) -> u64 {
    fs::metadata(path).unwrap().nlink()
}

/// Starts the real supervisor the way the controller does: the locked file is its stdin.
fn spawn_supervisor(job: &JobDir, lock: fs::File, env: &[(&str, &str)]) -> std::process::Child {
    let mut cmd = Command::new(SUPERVISOR_BIN);
    cmd.arg("run")
        .arg(&job.path)
        .stdin(Stdio::from(lock))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!("supervisor still running after {PATIENCE:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The listening sockets of this network namespace: TCP (v4, v6) in `LISTEN` and Unix
/// sockets in `LISTEN` (`/proc/net/unix` flag `00010000`).
fn listening_sockets() -> Vec<String> {
    let mut found = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in fs::read_to_string(file).unwrap_or_default().lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.get(3) == Some(&"0A") {
                found.push(format!("{file} {}", f[1]));
            }
        }
    }
    for line in fs::read_to_string("/proc/net/unix")
        .unwrap_or_default()
        .lines()
        .skip(1)
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.get(3) == Some(&"00010000") {
            found.push(format!("unix {}", f.get(7).unwrap_or(&"")));
        }
    }
    found.sort();
    found
}

// ---------------------------------------------------------------------------------------
// The guest boots.

/// Boots the image jailed by hand (the worker's launch, from the public pieces) and says
/// `Hello`: the guest answers `Ready` within 5 s, seeing the configured vCPUs and (as
/// `MemTotal`) the configured memory less what its kernel keeps.
#[tokio::test(flavor = "multi_thread")]
async fn real_guest_boots_and_answers_ready_within_5s() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    for vcpus in [1, 2] {
        let fx = Fx::new(&kvm);
        let cfg = FirecrackerConfig {
            vcpus,
            ..fx.cfg.clone()
        };
        let JailMode::Jailed(jc) = &cfg.jail else {
            unreachable!()
        };
        let dir = fx.path("boot");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(fx.task_dir()).unwrap();
        fs::File::create(fx.ws_img())
            .unwrap()
            .set_len(WS_IMAGE_BYTES)
            .unwrap();
        fs::File::create(dir.join("scratch.img"))
            .unwrap()
            .set_len(SCRATCH_IMAGE_BYTES)
            .unwrap();
        let id = AttemptId::new().to_string();
        let plan = jail::plan(jc, &cfg.firecracker_bin, &dir, &id).unwrap();
        fx.guard.watch(&id, &dir);
        let vm_json = render_vm_json(&cfg, &jail::chroot_view(&plan));
        let sources = StageSources {
            kernel: &cfg.image_dir.join("vmlinux"),
            rootfs: &cfg.image_dir.join("rootfs.squashfs"),
            ws_img: &fx.ws_img(),
            scratch_img: &dir.join("scratch.img"),
            vm_json: &vm_json,
        };
        jail::stage(jc, &plan, &dir, sources).unwrap();
        let started = Instant::now();
        let mut vm = Command::new(&jc.jailer_bin)
            .args(jail::jailer_args(
                jc,
                &plan,
                &cfg.firecracker_bin,
                cfg.vcpus,
                cfg.memory_mib,
                &cfg.resources,
            ))
            .env_clear()
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(fs::File::create(dir.join("console.log")).unwrap())
            .stderr(fs::File::create(dir.join("stderr.log")).unwrap())
            .spawn()
            .unwrap();
        let deadline = started + Duration::from_secs(15);
        let mut link = GuestLink::connect_until(&jail::host_uds(&plan), deadline, || {
            vm.try_wait().ok().flatten().map(|s| s.to_string())
        })
        .unwrap_or_else(|e| {
            panic!(
                "{e}; console: {}",
                fs::read_to_string(dir.join("console.log")).unwrap_or_default()
            )
        });
        let hello = Message::Hello {
            protocol: 1,
            attempt_token: mint_attempt_token(),
            task_id: fx.task.as_str().to_string(),
            effect_id: String::new(),
            attempt_id: id.clone(),
            lease_generation: 0,
            mode: Mode::Inspect,
        };
        let ready = link.hello(hello, deadline).unwrap();
        let took = started.elapsed();
        let Message::Ready {
            vcpus: seen,
            memory_mib,
            mode,
            protocol,
            ..
        } = ready
        else {
            panic!("{ready:?}")
        };
        println!("vcpus {vcpus}: Ready after {took:?}: vcpus {seen}, memory_mib {memory_mib}");
        assert!(took <= Duration::from_secs(5), "Ready after {took:?}");
        assert_eq!(
            (seen, mode, protocol),
            (vcpus, Mode::Inspect, 1),
            "Ready.vcpus equals worker_vcpus"
        );
        // MemTotal: the 256 MiB less the kernel's own reservation (about 26 MiB with this
        // kernel), so 10.5% below; within 15%, never above.
        assert!(
            (256 * 85 / 100..=256).contains(&memory_mib),
            "memory_mib {memory_mib} for 256 MiB"
        );
        // The cgroup carries the vCPU count.
        assert_eq!(
            fs::read_to_string(plan.cgroup.join("cpu.max"))
                .unwrap()
                .trim(),
            format!("{} 100000", vcpus * 100_000)
        );
        link.send(&Message::Shutdown).unwrap();
        assert!(matches!(
            link.recv(Instant::now() + Duration::from_secs(5)),
            Ok(Message::Bye)
        ));
        drop(link);
        let until = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = vm.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < until,
                "Firecracker still running 5 s after Shutdown"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "the guest's reboot ends Firecracker with 0: {status}"
        );
        let collected = jail::collect(&dir, &kvm.cgroup_root).unwrap();
        assert!(
            collected.cgroup_removed && collected.jail_removed,
            "{collected:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_digest_from_the_real_guest_equals_the_host_digest() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    let ran = fx.snapshot().await;
    let host = workspace_digest(&fx.path("snapshot")).unwrap();
    assert_eq!(
        host.to_string(),
        "be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8",
        "the README's digest"
    );
    assert_eq!(
        ran.out.new_workspace,
        Some(host),
        "the guest's SnapshotDone digest"
    );
    assert_eq!(
        fx.digest(),
        host,
        "an inspection boot of ws.img answers it too"
    );
    // And the tree in the ext4 image, read on the host with debugfs, digests the same.
    let dumped = common::dump_ws_img(&fx.ws_img(), &fx.path("dumps"));
    assert_eq!(
        workspace_digest(&dumped).unwrap(),
        host,
        "the tree the guest wrote"
    );
}

// ---------------------------------------------------------------------------------------
// The threat model: one hostile profile per row of the spec's table.

/// The guest's `python3` is the interpreter its image manifest records (an image with an
/// `interpreter` entry, such as `python-stdlib-py314-v1`); the manifest's claim alone proves
/// nothing about what was copied into the rootfs.
#[tokio::test(flavor = "multi_thread")]
async fn the_guest_interpreter_is_the_one_the_image_manifest_records() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script(
        "import json, platform, sys; \
         print(json.dumps({'version': platform.python_version(), 'executable': sys.executable}))",
    );
    let v = evidence(&fx.run(EffectKind::RunVerification).await.out);
    assert_eq!(v["passed"], true, "{v}");
    let found = findings(&v);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(kvm.image_dir.join("image.json")).unwrap()).unwrap();
    println!("image {}: guest python {found}", manifest["id"]);
    match manifest["interpreter"]["version"].as_str() {
        Some(version) => assert_eq!(found["version"], version, "{found}"),
        None => assert!(
            found["version"]
                .as_str()
                .is_some_and(|v| v.starts_with("3.")),
            "{found}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn net_probe_cannot_reach_anything_and_sees_only_lo() {
    let Some(kvm) = kvm::require() else { return };
    // Alone: the listening sockets of the network namespace are compared across the run.
    let _alone = exclusive().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/net-probe");
    let before = listening_sockets();
    let ran = fx.run(EffectKind::RunVerification).await;
    let after = listening_sockets();
    let v = evidence(&ran.out);
    let f = findings(&v);
    assert_eq!(v["passed"], true, "{v}");
    assert_eq!(f["net"], "unreachable", "{f}");
    assert_eq!(f["public"], "unreachable", "{f}");
    assert_eq!(f["ifaces"], serde_json::json!(["lo"]), "{f}");
    assert_eq!(
        after, before,
        "the run left a listening socket behind (or took one away)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_probe_finds_no_host_secret_anywhere() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    // A whole task through the real supervisor, whose environment (and its worker's) holds
    // the secret; the home holds it in a file too.
    let env = Env::new(10);
    let root = kvm.root();
    let _guard = HomeGuard::new(root.path(), &kvm.cgroup_root);
    copy_dir(
        &env.dir.path().join("snapshot"),
        &root.path().join("snapshot"),
    );
    copy_dir(
        &fixtures().join("profiles/hostile/secret-probe"),
        &root.path().join("profile"),
    );
    let secret = mint_attempt_token();
    fs::write(
        root.path().join("secret.txt"),
        format!("AGENTOS_TEST_SECRET={secret}\n"),
    )
    .unwrap();
    let cfg = kvm.jailed_config(root.path());
    let exec = supervised(
        &root.path().join("jobs"),
        WorkerConfig::Firecracker(cfg),
        &ExecCounts::default(),
        None,
        &[],
    )
    .with_env("AGENTOS_TEST_SECRET", secret.clone());
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded, "{:?}", env.event_types());

    let verify = env.effects("RunVerification").remove(0);
    let v = env.blob_json(&verify.result_digest.unwrap());
    let f = findings(&v);
    assert_eq!(v["passed"], true, "{v}");
    assert_eq!(f["hits"], serde_json::json!([]), "{f}");
    assert_eq!(
        (f["vsock"].clone(), f["blockdev"].clone()),
        (serde_json::json!("EACCES"), serde_json::json!("EACCES")),
        "{f}"
    );
    assert_eq!(
        f["vsock_connect"], "ECONNRESET",
        "a guest-initiated connection to the host is reset: {f}"
    );
    assert_eq!(
        f["fds"],
        serde_json::json!([0, 1, 2]),
        "the check holds no descriptor beyond 0-2: {f}"
    );
    assert_eq!(
        (
            f["uid"].clone(),
            f["no_new_privs"].clone(),
            f["setuid0"].clone()
        ),
        (1001.into(), 1.into(), "EPERM".into()),
        "{f}"
    );
    assert_eq!(
        (f["nproc"].clone(), f["nofile"].clone()),
        (
            serde_json::json!([256, 256]),
            serde_json::json!([1024, 1024])
        ),
        "{f}"
    );
    assert_eq!(f["oom_score_adj"], 1000, "{f}");
    assert!(
        f["files_read"].as_u64().unwrap() > 1000 && f["truncated"] == false,
        "the walk read the filesystems: {f}"
    );

    // Nowhere on the way out either: no job's logs, no artifact, no exported file.
    let bundle = root.path().join("bundle");
    agentos_engine::export::export_bundle(&env.db, &env.blobs, &env.task, &bundle).unwrap();
    let mut places = vec![(PathBuf::from("evidence"), v.to_string().into_bytes())];
    for dir in [root.path().join("jobs"), bundle] {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let ty = e.file_type().unwrap();
                if ty.is_dir() {
                    stack.push(e.path());
                } else if ty.is_file() && e.metadata().unwrap().len() < 64 << 20 {
                    places.push((e.path(), fs::read(e.path()).unwrap()));
                }
            }
        }
    }
    assert!(
        places.iter().any(|(p, _)| p.ends_with("console.log")),
        "the jobs' logs were searched"
    );
    for (path, bytes) in places {
        let found = bytes.windows(secret.len()).any(|w| w == secret.as_bytes());
        assert!(!found, "the secret is in {}", path.display());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_burn_is_bounded_by_vcpu_count() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/cpu-burn");
    let (ran, samples) = fx.verify_watched(Duration::from_millis(100), |w| w).await;
    let v = evidence(&ran.out);
    assert_eq!(
        (v["passed"].clone(), v["exit_code"].clone()),
        (serde_json::json!(true), serde_json::json!(0)),
        "the check exits on its own: {v}"
    );
    assert_eq!(
        (
            findings(&v)["nproc"].clone(),
            findings(&v)["burners"].clone()
        ),
        (1.into(), 8.into()),
        "{v}"
    );
    let cpu: Vec<(Duration, u64)> = samples
        .iter()
        .filter_map(|s| Some((s.at, s.cpu_ticks?)))
        .collect();
    let ((t0, c0), (t1, c1)) = (cpu[0], *cpu.last().unwrap());
    let wall = (t1 - t0).as_secs_f64();
    let used = (c1 - c0) as f64 / CLOCK_TICKS as f64;
    println!(
        "cpu-burn: Firecracker used {used:.2} s of CPU in {wall:.2} s with 1 vCPU and 8 burners"
    );
    assert!(wall >= 2.0, "the window covers the burn: {wall}");
    assert!(used <= 1.25 * wall, "{used:.2} s of CPU in {wall:.2} s");
    // And the burn happened: the vCPU was busy for most of the window (boot and shutdown
    // dilute it; unthrottled runs measure about 87%).
    assert!(used >= 0.5 * wall, "only {used:.2} s of CPU in {wall:.2} s");
}

#[tokio::test(flavor = "multi_thread")]
async fn mem_hog_is_oom_killed_and_the_agent_survives() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/mem-hog");
    let (ran, samples) = fx.verify_watched(Duration::from_millis(100), |w| w).await;
    // A complete Verified: the agent survived to report the check's death.
    let v = evidence(&ran.out);
    assert_eq!(
        (v["exit_code"].clone(), v["passed"].clone()),
        (serde_json::Value::Null, serde_json::json!(false)),
        "{v}"
    );
    let f = findings(&v);
    assert_eq!(
        (
            f["write_-1000"].clone(),
            f["write_0"].clone(),
            f["oom_score_adj"].clone()
        ),
        ("EACCES".into(), "EACCES".into(), 1000.into()),
        "the check could not lower its OOM priority: {f}"
    );
    assert!(
        ran.log("console.log")
            .contains("Out of memory: Killed process"),
        "the guest kernel's OOM killer: {}",
        ran.log("console.log")
    );
    // The guest bound it, not the host: Firecracker stayed within the VM's memory and its
    // cgroup never OOM-killed anything.
    assert!(samples.len() >= 3, "{samples:?}");
    for s in &samples {
        if let Some(rss) = s.rss_kib {
            assert!(rss <= (256 + 96) * 1024, "VmRSS {rss} KiB at {:?}", s.at);
        }
        if let Some(events) = s.cgroup.get("memory.events") {
            assert_eq!(
                keyed(events).get("oom_kill"),
                Some(&0),
                "host-side OOM kill at {:?}: {events}",
                s.at
            );
        }
    }
    // The next effect's VM boots and works.
    let next = fx.run(EffectKind::ReadSnapshot).await;
    assert_eq!(
        next.out.receipt.outcome,
        Outcome::Success,
        "{}",
        out_text(&next.out)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_bomb_fails_and_leaves_no_process() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/fork-bomb");
    let ran = fx.run(EffectKind::RunVerification).await;
    // The check exits 1 by design, so a failing outcome alone proves nothing: what this test
    // proves is the clean shutdown and the empty `/proc` afterwards; the host-side bound
    // (the bomb never adds a host process) is `fork_bomb_never_adds_a_host_process`.
    match &ran.out.receipt.outcome {
        Outcome::Failure(reason) => assert_eq!(reason, "timeout"),
        Outcome::Success => {
            let v = evidence(&ran.out);
            assert_eq!(v["passed"], false, "{v}");
            assert_ne!(v["exit_code"], 0, "{v}");
        }
    }
    // The guest shut down cleanly on `Shutdown` (it was not killed), and nothing is left.
    let console = ran.log("console.log");
    assert!(console.contains("powering off: Shutdown"), "{console}");
    assert!(ran.took < Duration::from_secs(5 + 15 + 5), "{:?}", ran.took);
    fx.assert_nothing_left_within(Duration::from_secs(1));
    assert!(
        !fx.cgroup_of(&ran.ctx).exists() && !ran.job.path.join("jail").exists(),
        "the jail was collected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_fill_is_bounded_by_the_images_and_the_root_stays_read_only() {
    let Some(kvm) = kvm::require() else { return };
    // Alone (see the report's concern): the drives' host page cache is charged to the jail's
    // cgroup, and with every other VM of the tier writing too, a page-cache charge at the
    // full `memory.max` (guest + 128 MiB) has met the cgroup's OOM killer.
    let _alone = exclusive().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/disk-fill");
    // The images as the host sees them while the check fills them: (apparent, allocated).
    let (ws, jobs) = (fx.ws_img(), fx.path("jobs"));
    let sizes = sample(Duration::from_millis(50), move |_| {
        let scratch = fs::read_dir(&jobs)
            .ok()?
            .flatten()
            .map(|e| e.path().join("scratch.img"))
            .find(|p| p.exists())?;
        let (w, s) = (fs::metadata(&ws).ok()?, fs::metadata(&scratch).ok()?);
        Some(((w.len(), w.blocks() * 512), (s.len(), s.blocks() * 512)))
    });
    let (ran, samples) = fx.verify_watched(Duration::from_millis(20), |w| w).await;
    let sizes = sizes.stop();
    if std::env::var_os("KVM_DEBUG").is_some() || ran.out.receipt.outcome != Outcome::Success {
        println!("console: {}", ran.log("console.log"));
        for s in &samples {
            let stat = keyed(s.cgroup.get("memory.stat").map_or("", String::as_str));
            let pick = |k: &str| stat.get(k).copied().unwrap_or(0) >> 20;
            println!(
                "{:?} rss {:?} current {} anon {} file {} dirty {} writeback {} events {:?}",
                s.at,
                s.rss_kib.map(|k| k >> 10),
                s.cgroup
                    .get("memory.current")
                    .map_or(0, |c| c.trim().parse::<u64>().unwrap_or(0) >> 20),
                pick("anon"),
                pick("file"),
                pick("file_dirty"),
                pick("file_writeback"),
                s.cgroup.get("memory.events").map(|e| keyed(e))
            );
        }
    }
    let v = evidence(&ran.out);
    let f = findings(&v);
    assert_eq!(v["passed"], true, "{v}");
    assert_eq!(
        (f["tmp"].clone(), f["scratch"].clone()),
        ("ENOSPC".into(), "ENOSPC".into()),
        "{f}"
    );
    assert_eq!(
        f["workspace"], "EACCES",
        "/workspace is not writable by the check: {f}"
    );
    assert_eq!(f["workspace_owner"], 1000, "{f}");
    assert_eq!(
        (f["root_ro"].clone(), f["root_ro_after"].clone()),
        (true.into(), true.into()),
        "{f}"
    );
    assert_eq!(f["remount_syscall"], "EPERM", "{f}");
    assert_ne!(f["remount_rc"], 0, "{f}");
    assert!(!sizes.is_empty(), "the images were sampled");
    let max_scratch = sizes.iter().map(|(_, s)| s.1).max().unwrap();
    for ((ws_len, ws_alloc), (scratch_len, scratch_alloc)) in &sizes {
        assert_eq!(
            (*ws_len, *scratch_len),
            (WS_IMAGE_BYTES, SCRATCH_IMAGE_BYTES),
            "apparent sizes stay the constants"
        );
        assert!(
            *ws_alloc <= WS_IMAGE_BYTES && *scratch_alloc <= SCRATCH_IMAGE_BYTES,
            "allocated {ws_alloc} / {scratch_alloc}"
        );
    }
    println!(
        "disk-fill: scratch.img allocated up to {max_scratch} bytes; check wrote {} to scratch",
        f["scratch_bytes"]
    );
    assert!(
        max_scratch >= f["scratch_bytes"].as_u64().unwrap() / 2,
        "the fill reached the host file (sampled {max_scratch})"
    );
}

fn resources(
    disk_mib: u32,
    scratch_mib: u32,
    bandwidth: Option<u32>,
    iops: Option<u32>,
) -> VmResources {
    VmResources {
        version: 1,
        disk_mib,
        scratch_mib,
        bandwidth_mib_s: bandwidth,
        iops,
    }
}

/// The guest's drives have the recorded sizes, both above the version-0 constants, so the
/// jail's file-size limit must cover a workspace image over 1 GiB; and the check fills
/// scratch up to its larger size before ENOSPC.
#[tokio::test(flavor = "multi_thread")]
async fn the_guest_sees_and_fills_the_contracted_drive_sizes() {
    let Some(kvm) = kvm::require() else { return };
    let _alone = exclusive().await;
    let fx = Fx::new(&kvm).with_resources(resources(1536, 768, None, None));
    let ran = fx.snapshot().await;
    assert_eq!(
        ran.out.receipt.outcome,
        Outcome::Success,
        "{}",
        out_text(&ran.out)
    );
    assert_eq!(fs::metadata(fx.ws_img()).unwrap().len(), 1536 << 20);
    fx.use_script(
        "import json; \
         print(json.dumps({d: int(open(f'/sys/block/{d}/size').read()) * 512 for d in ('vdb', 'vdc')}))",
    );
    let v = evidence(&fx.run(EffectKind::RunVerification).await.out);
    assert_eq!(v["passed"], true, "{v}");
    let sizes = findings(&v);
    assert_eq!(
        (sizes["vdb"].as_u64(), sizes["vdc"].as_u64()),
        (Some(1536 << 20), Some(768 << 20)),
        "{sizes}"
    );
    fx.use_profile("hostile/disk-fill");
    let v = evidence(&fx.run(EffectKind::RunVerification).await.out);
    let f = findings(&v);
    assert_eq!(f["scratch"], "ENOSPC", "{f}");
    let wrote = f["scratch_bytes"].as_u64().unwrap();
    assert!(
        wrote > 512 << 20 && wrote < 768 << 20,
        "filled {wrote} bytes of a 768 MiB scratch drive"
    );
}

/// At the minimum contracted bandwidth, 32 MiB/s per writable drive, 160 MiB written and
/// synced to scratch takes at least (160 - 32) / 32 = 4 s: the bucket starts full with one
/// second's worth. Only a lower bound is asserted; an upper bound would depend on the host.
#[tokio::test(flavor = "multi_thread")]
async fn the_bandwidth_limit_bites_on_synced_writes() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm).with_resources(resources(1024, 512, Some(32), None));
    fx.snapshot().await;
    fx.use_script(
        "import json, os, time; \\
         block = b'x' * (1 << 20); \\
         fd = os.open('/scratch/check/rate', os.O_WRONLY | os.O_CREAT, 0o600); \\
         start = time.monotonic(); \\
         [os.write(fd, block) for _ in range(160)]; \\
         os.fsync(fd); \\
         print(json.dumps({'seconds': time.monotonic() - start}))",
    );
    let v = evidence(&fx.run(EffectKind::RunVerification).await.out);
    assert_eq!(v["passed"], true, "{v}");
    let seconds = findings(&v)["seconds"].as_f64().unwrap();
    println!("160 MiB synced at 32 MiB/s in {seconds:.2}s");
    assert!(seconds >= 3.5, "the limit did not bite: {seconds:.2}s");
}

/// The lowest contracted rates still boot, format scratch, snapshot, verify and inspect the
/// fixture within the timeouts.
#[tokio::test(flavor = "multi_thread")]
async fn the_minimum_rates_still_run_the_fixture() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let (bandwidth, iops) = (
        *agentos_core::contract::WORKER_DISK_BANDWIDTH_MIB_S.start(),
        *agentos_core::contract::WORKER_DISK_IOPS.start(),
    );
    let fx = Fx::new(&kvm).with_resources(resources(1024, 512, Some(bandwidth), Some(iops)));
    let started = Instant::now();
    let ran = fx.snapshot().await;
    assert_eq!(
        ran.out.receipt.outcome,
        Outcome::Success,
        "{}",
        out_text(&ran.out)
    );
    let snapshot = started.elapsed();
    // The unfixed fixture: the check runs to the end and finds the bug.
    let v = evidence(&fx.run(EffectKind::RunVerification).await.out);
    assert_eq!(
        (v["exit_code"].clone(), v["passed"].clone()),
        (1.into(), false.into()),
        "{v}"
    );
    let digest = fx.digest();
    println!(
        "at {bandwidth} MiB/s and {iops} ops/s: snapshot {snapshot:?}, all {:?}, digest {digest}",
        started.elapsed()
    );
}

/// The minimum rates were chosen so that a snapshot near the limits (65,000 files, 243 MiB)
/// still fits the 120 s reply and the 60 s inspection deadlines; measured on the reference
/// host at 45 s and 22 s (see the VM resources design).
#[tokio::test(flavor = "multi_thread")]
async fn the_minimum_rates_fit_a_near_limit_snapshot_within_the_deadlines() {
    let Some(kvm) = kvm::require() else { return };
    let _alone = exclusive().await;
    let (bandwidth, iops) = (
        *agentos_core::contract::WORKER_DISK_BANDWIDTH_MIB_S.start(),
        *agentos_core::contract::WORKER_DISK_IOPS.start(),
    );
    let fx = Fx::new(&kvm).with_resources(resources(1024, 512, Some(bandwidth), Some(iops)));
    let big = fx.path("snapshot/big");
    let mut seed = 1u64;
    for i in 0..45 {
        let block: Vec<u8> = (0..4 << 20)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 56) as u8
            })
            .collect();
        fs::create_dir_all(&big).unwrap();
        fs::write(big.join(format!("blob-{i}")), block).unwrap();
    }
    for d in 0..65 {
        let dir = big.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..1000 {
            fs::write(dir.join(format!("f{f}")), [b'a'; 1024]).unwrap();
        }
    }
    let started = Instant::now();
    let ran = fx.run(EffectKind::ReadSnapshot).await;
    assert_eq!(
        ran.out.receipt.outcome,
        Outcome::Success,
        "{}",
        out_text(&ran.out)
    );
    let snapshot = started.elapsed();
    let started = Instant::now();
    assert_eq!(fx.digest(), ran.out.new_workspace.unwrap());
    println!(
        "near-limit snapshot at {bandwidth} MiB/s and {iops} ops/s: {snapshot:?}, inspection {:?}",
        started.elapsed()
    );
}

/// A directory with a small tmpfs of its own mounted on it, so the host really runs out of
/// space. Unmounted and removed on drop, which must come after the fixture inside it.
struct SmallHostDisk {
    dir: PathBuf,
}

impl SmallHostDisk {
    fn new(kvm: &kvm::Kvm, mib: u64) -> (SmallHostDisk, TempDir) {
        let root = kvm.root();
        let status = std::process::Command::new("mount")
            .args([
                "-t",
                "tmpfs",
                "-o",
                &format!("size={mib}m,mode=0755"),
                "tmpfs",
            ])
            .arg(root.path())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "mount a {mib} MiB tmpfs (the KVM tier has CAP_SYS_ADMIN)"
        );
        (
            SmallHostDisk {
                dir: root.path().to_path_buf(),
            },
            root,
        )
    }

    fn free_mib(&self) -> u64 {
        let st = rustix::fs::statvfs(&self.dir).unwrap();
        (st.f_bavail * st.f_frsize) >> 20
    }

    /// Fills the disk up to `leave_mib` MiB free with a host file.
    fn fill_leaving(&self, leave_mib: u64) {
        fill_leaving(&self.dir, leave_mib);
    }
}

fn fill_leaving(dir: &Path, leave_mib: u64) {
    let st = rustix::fs::statvfs(dir).unwrap();
    let fill = ((st.f_bavail * st.f_frsize) >> 20).saturating_sub(leave_mib);
    let mut f = fs::File::options()
        .create(true)
        .append(true)
        .open(dir.join("balloon"))
        .unwrap();
    let block = vec![0x5au8; 1 << 20];
    for _ in 0..fill {
        if std::io::Write::write_all(&mut f, &block).is_err() {
            break;
        }
    }
}

impl Drop for SmallHostDisk {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount")
            .arg("-l")
            .arg(&self.dir)
            .status();
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Past the advisory free-space check, as if space disappeared right after it.
fn past_the_space_check(w: FirecrackerWorker) -> FirecrackerWorker {
    let mut env = test_env();
    env.push(("AGENTOS_TEST_HOST_FREE_MIB".into(), "1000000".into()));
    w.with_env(env)
}

fn io_errors_in(ran: &Ran) -> bool {
    let console = ran.log("console.log");
    for line in console.lines().filter(|l| l.contains("error")).take(6) {
        println!("  console: {line}");
    }
    console.contains("I/O error")
}

/// A successful snapshot on a host whose free space the advisory check would refuse.
async fn snapshot_past_the_space_check(fx: &Fx) -> Digest {
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let ran = fx
        .run_with(&req, &fx.cfg.clone(), past_the_space_check)
        .await;
    assert_eq!(
        ran.out.receipt.outcome,
        Outcome::Success,
        "{}",
        out_text(&ran.out)
    );
    ran.out.new_workspace.unwrap()
}

/// A host ENOSPC while the guest writes its drives never yields a success that is not on
/// the image: during a snapshot, a patch and a verification's scratch writes.
#[tokio::test(flavor = "multi_thread")]
async fn host_enospc_under_the_drives_never_yields_an_unbacked_success() {
    let Some(kvm) = kvm::require() else { return };
    let _alone = exclusive().await;

    // 1. Snapshot: 150 MiB of incompressible data into a workspace on a nearly full host.
    let (disk, root) = SmallHostDisk::new(&kvm, 200);
    let mut fx = Fx::new_in(&kvm, root);
    let outside = tempfile::tempdir().unwrap();
    copy_dir(&fixtures().join("parser-repo"), outside.path());
    let mut seed = 7u64;
    for i in 0..3 {
        // Under the 64 MiB per-file snapshot limit.
        let noise: Vec<u8> = (0..50 << 20)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 56) as u8
            })
            .collect();
        fs::write(outside.path().join(format!("noise-{i}.bin")), noise).unwrap();
    }
    fx.cfg.snapshot_dir = outside.path().to_path_buf();
    println!("snapshot: {} MiB free before", disk.free_mib());
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let ran = fx
        .run_with(&req, &fx.cfg.clone(), past_the_space_check)
        .await;
    println!(
        "snapshot under ENOSPC: {:?} new_workspace {:?} unresolved {} console I/O errors {}",
        ran.out.receipt.outcome,
        ran.out.new_workspace,
        ran.out.unresolved,
        io_errors_in(&ran)
    );
    let why = failure(&ran.out);
    assert!(
        why.starts_with("host disk: the VM's drives returned I/O errors"),
        "{why}"
    );
    assert_eq!(ran.out.new_workspace, None);
    drop(fx);
    drop(disk);

    // 2. Patch: a snapshot first, then the host fills up before the fix is applied.
    let (disk, root) = SmallHostDisk::new(&kvm, 200);
    let fx = Fx::new_in(&kvm, root);
    let base = snapshot_past_the_space_check(&fx).await;
    // A patch adding a 3.6 MB file (under the 4 MiB patch limit), with less room than that
    // left on the host once the VM has booted and formatted scratch.
    let lines = 60_000;
    let mut big = format!("--- /dev/null\n+++ b/src/big.txt\n@@ -0,0 +1,{lines} @@\n");
    for i in 0..lines {
        big.push_str(&format!("+{i:059}\n"));
    }
    let req = fx.request(
        EffectKind::ApplyPatch {
            expected_base: base,
        },
        big.as_bytes(),
    );
    // The host fills up once the VM has booted (about 0.6 s), while the patch is applied.
    let dir = disk.dir.clone();
    let balloon = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        fill_leaving(&dir, 0);
    });
    let ran = fx
        .run_with(&req, &fx.cfg.clone(), past_the_space_check)
        .await;
    balloon.join().unwrap();
    println!(
        "patch under ENOSPC: {:?} new_workspace {:?} unresolved {} console I/O errors {}",
        ran.out.receipt.outcome,
        ran.out.new_workspace,
        ran.out.unresolved,
        io_errors_in(&ran)
    );
    if io_errors_in(&ran) && ran.out.receipt.outcome != Outcome::Success {
        let why = failure(&ran.out);
        assert!(why.starts_with("host disk: "), "{why}");
    }
    let _ = fs::remove_file(disk.dir.join("balloon"));
    let on_image = fx.digest();
    match (&ran.out.receipt.outcome, ran.out.new_workspace) {
        // A reported success is what the image holds.
        (Outcome::Success, Some(reported)) => assert_eq!(on_image, reported),
        // A failure leaves the base, or an image every later patch refuses as a conflict.
        _ if on_image == base => println!("patch under ENOSPC: the image is still the base"),
        _ => {
            let again = fx
                .run_with(
                    &fx.request(
                        EffectKind::ApplyPatch {
                            expected_base: base,
                        },
                        fix_patch().as_bytes(),
                    ),
                    &fx.cfg.clone(),
                    |w| w,
                )
                .await;
            let why = failure(&again.out);
            assert!(why.contains("version conflict"), "{why}");
            println!("patch under ENOSPC: the image moved; the next patch is a version conflict");
        }
    }
    drop(fx);
    drop(disk);

    // 3. Verification: the check writes and syncs 100 MiB to scratch on a nearly full host.
    let (disk, root) = SmallHostDisk::new(&kvm, 200);
    let fx = Fx::new_in(&kvm, root);
    snapshot_past_the_space_check(&fx).await;
    fx.use_script(
        "import os, sys; \\
         block = b'x' * (1 << 20); \\
         fd = os.open('/scratch/check/fill', os.O_WRONLY | os.O_CREAT, 0o600); \\
         [os.write(fd, block) for _ in range(100)]; \\
         os.fsync(fd); \\
         print('PASSED')",
    );
    disk.fill_leaving(30);
    let req = fx.request(EffectKind::RunVerification, b"");
    let ran = fx
        .run_with(&req, &fx.cfg.clone(), past_the_space_check)
        .await;
    println!(
        "verification under ENOSPC: {} console I/O errors {}",
        out_text(&ran.out),
        io_errors_in(&ran)
    );
    // Reported as the host's failure, not as a failing check the agent would try to fix.
    let why = failure(&ran.out);
    assert!(
        why.starts_with("host disk: the VM's drives returned I/O errors")
            && why.contains("is not evidence"),
        "{why}"
    );
    drop(fx);
    drop(disk);
}

/// The host reads guest block I/O errors from the serial console, so the check must not be
/// able to write there (or into the kernel log) to disguise its own failure as the host's.
#[tokio::test(flavor = "multi_thread")]
async fn the_check_cannot_write_the_console_or_the_kernel_log() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    let profile = fx.path("profile");
    fs::remove_dir_all(&profile).unwrap();
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("profile.json"),
        r#"{"id":"console-probe","command":["python3","probe.py"],"protected":true}"#,
    )
    .unwrap();
    fs::write(
        profile.join("probe.py"),
        r#"import errno, json
found = {}
for path in ["/dev/console", "/dev/ttyS0", "/dev/kmsg", "/dev/tty0"]:
    try:
        with open(path, "w") as f:
            f.write("[    1.0] I/O error, dev vdc, sector 1\n")
        found[path] = "written"
    except OSError as e:
        found[path] = errno.errorcode.get(e.errno, str(e))
print(json.dumps(found))
"#,
    )
    .unwrap();
    let ran = fx.run(EffectKind::RunVerification).await;
    let v = evidence(&ran.out);
    let f = findings(&v);
    println!("console probe: {f}");
    for p in ["/dev/console", "/dev/ttyS0", "/dev/kmsg", "/dev/tty0"] {
        assert_ne!(f[p], "written", "the check wrote {p}: {f}");
    }
    assert!(
        !ran.log("console.log").contains("dev vdc, sector 1"),
        "{}",
        ran.log("console.log")
    );
}

/// A pinned profile whose source changed, a source changed while the check runs, and a
/// check that rewrites its own staged copy: none of them ever yields passing evidence.
#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_staged_profile_never_passes_in_the_real_guest() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;

    // Pinned, then the source changed: the guest's digest of what it staged is not the pin.
    let pinned = FirecrackerConfig {
        profile_digest: Some(workspace_digest(&fx.path("profile")).unwrap()),
        ..fx.cfg.clone()
    };
    fs::write(fx.path("profile/check_parser.py"), b"print('PASSED')\n").unwrap();
    let req = fx.request(EffectKind::RunVerification, b"");
    let ran = fx.run_with(&req, &pinned, |w| w).await;
    assert!(
        failure(&ran.out).starts_with("profile digest mismatch: pinned "),
        "{}",
        out_text(&ran.out)
    );

    // Unpinned, the source changes once the VM is up (after the host digested it).
    // The host cannot see into the guest, so "while the check runs" is by the clock: the
    // check sleeps 10 s, the change comes 4 s after Firecracker started (a boot and the
    // profile's transfer take under 1 s). A change before the transfer ends instead
    // breaks the announced stream: the guest drops the connection (also a failure).
    fx.use_script("import time; time.sleep(10); print('PASSED')");
    let req = fx.request(EffectKind::RunVerification, b"");
    let root = fx.root().to_path_buf();
    let profile = fx.path("profile/profile.json");
    let tamper = thread::spawn(move || {
        wait_for("the VM", || !home_firecrackers(&root).is_empty());
        thread::sleep(Duration::from_secs(4));
        fs::write(
            profile,
            r#"{"id":"kvm","command":["python3","-c","print('PASSED')"],"protected":true}"#,
        )
        .unwrap();
    });
    let ran = fx.run_with(&req, &fx.cfg.clone(), |w| w).await;
    tamper.join().unwrap();
    assert_eq!(
        failure(&ran.out),
        "protected profile changed during verification"
    );

    // The check tries to rewrite its own staged profile (root's, read-only to it), then
    // exits 0: the staged copy is untouched and the evidence names the staged digest.
    fx.use_script(
        "import errno, os\ntry:\n    open('profile.json', 'a').write(' ')\n    print('rewrote')\nexcept OSError as e:\n    print(errno.errorcode[e.errno])",
    );
    let source = workspace_digest(&fx.path("profile")).unwrap();
    let ran = fx.run(EffectKind::RunVerification).await;
    let v = evidence(&ran.out);
    assert_eq!(v["summary"], "EACCES", "{v}");
    assert_eq!(v["profile_digest"], source.to_string(), "{v}");
}

// ---------------------------------------------------------------------------------------
// Crashes and kills with a real VM.

/// The VM dies right after `ApplyPatch` was sent and the controller dies right after the
/// launch: the image holds the base or the base plus the patch, the supervisor writes no
/// receipt, recovery's inspection says which (`PublishReconciled` / `Redispatch`), and the
/// task ends SUCCEEDED with the digest a fresh inspection reads from `ws.img`.
#[tokio::test(flavor = "multi_thread")]
async fn inspection_after_a_killed_patch_vm_converges_to_succeeded() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let env = Env::new(10);
    let root = kvm.root();
    let _guard = HomeGuard::new(root.path(), &kvm.cgroup_root);
    copy_dir(
        &env.dir.path().join("snapshot"),
        &root.path().join("snapshot"),
    );
    copy_dir(
        &env.dir.path().join("profile"),
        &root.path().join("profile"),
    );
    let cfg = kvm.jailed_config(root.path());
    let jobs = root.path().join("jobs");
    let worker = WorkerConfig::Firecracker(cfg.clone());
    let hook = CrashHook::at(CrashPoint::DuringExecute, "apply_patch");
    let dying = supervised(
        &jobs,
        worker.clone(),
        &ExecCounts::default(),
        Some(hook.clone()),
        &[
            (TEST_WORKERS_ENV, "1"),
            ("AGENTOS_TEST_KILL_VM_AFTER_REQUEST", "1"),
        ],
    );
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let err = run_task_with(
        &env.db,
        &env.blobs,
        &dying,
        &mut agent,
        &env.task,
        &RunOptions::crash_with(hook),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, EngineError::Crashed(CrashPoint::DuringExecute)),
        "{err:?}"
    );
    let patch = env.effects("ApplyPatch").remove(0);
    let job = JobDir::list(&jobs, &patch.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    wait_for_async("the patch job to end", || {
        job.read_status()
            .is_some_and(|s| s.state != JobState::Running)
    })
    .await;
    assert!(
        job.read_outcome().is_none() && job.read_receipt().is_none(),
        "no outcome, no receipt"
    );

    let inspector = Inspector::new(cfg.clone(), root.path().join("inspect")).with_env(test_env());
    let Answer::Digest(on_disk) = inspector.query(&env.task, Query::Digest).unwrap() else {
        panic!()
    };
    let base = workspace_digest(&root.path().join("snapshot")).unwrap();
    let patch_file = root.path().join("fix.patch");
    fs::write(&patch_file, fix_patch()).unwrap();
    let patched_tree = tempfile::tempdir().unwrap();
    copy_dir(&root.path().join("snapshot"), patched_tree.path());
    let applied = Command::new("git")
        .arg("apply")
        .arg(&patch_file)
        .current_dir(patched_tree.path())
        .status()
        .unwrap();
    assert!(applied.success());
    let with_patch = workspace_digest(patched_tree.path()).unwrap();
    assert!(
        on_disk == base || on_disk == with_patch,
        "ws.img holds {on_disk}: neither the base nor the base plus the patch"
    );

    let exec = supervised(&jobs, worker, &ExecCounts::default(), None, &[]);
    let report = recover(&env.db, &env.blobs, &exec, &env.task)
        .await
        .unwrap();
    let decision = report
        .decisions
        .iter()
        .find(|d| d.effect_id == patch.effect_id)
        .unwrap_or_else(|| panic!("{report:?}"));
    let expected = if on_disk == base {
        Decision::Redispatch
    } else {
        Decision::PublishReconciled
    };
    assert_eq!(
        decision.decision, expected,
        "{decision:?} with ws.img at {on_disk}"
    );
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded, "{:?}", env.event_types());
    let task = env.db.task(&env.task).unwrap();
    let Answer::Digest(now) = inspector.query(&env.task, Query::Digest).unwrap() else {
        panic!()
    };
    assert_eq!(
        task.workspace_digest, now,
        "the journal's digest is what ws.img holds"
    );
    assert_eq!(task.verified_digest, Some(now));
    assert_eq!(now, with_patch);
    assert!(home_firecrackers(root.path()).is_empty());
}

/// A verification whose check hangs: the lease ends the supervisor's job, and the VM is
/// gone with the worker.
#[tokio::test(flavor = "multi_thread")]
async fn vm_dies_with_the_worker_on_lease_expiry() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script("import time; time.sleep(30)");
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let (job, lock) = JobDir::create(
        &fx.path("jobs"),
        &fx.job_request(&req, &c, &fx.cfg, Some(2_500)),
    )
    .unwrap();
    let mut supervisor = spawn_supervisor(&job, lock, &[]);
    let id = c.attempt_id.to_string();
    wait_for("the VM", || {
        firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(id.as_str()))
    });
    assert!(wait_exit(&mut supervisor).success());
    let status = job.read_status().unwrap();
    assert_eq!(
        (status.state, status.reason),
        (JobState::Killed, Some(KillReason::Lease)),
        "{status:?}"
    );
    assert_eq!(failure(&job.read_receipt().unwrap()), "lease expired");
    // The VM was killed with the worker's group: gone within 2 s of the supervisor's exit.
    fx.assert_no_vm_within(Duration::from_secs(2));
    fx.assert_nothing_left_within(Duration::from_secs(2));
    // Its jail is left for the controller, which collects it once the job is settled.
    let cgroup = fx.cgroup_of(&c);
    assert!(
        cgroup.is_dir() && job.path.join("jail").is_dir(),
        "the jail waits for the controller"
    );
    assert!(
        fx.executor(None, &[])
            .fence_jobs(std::slice::from_ref(&job))
            .await
    );
    assert!(
        !cgroup.exists() && !job.path.join("jail").exists(),
        "collected by the controller"
    );
}

/// The controller dies after the snapshot completed; on resume it asks the inspector for
/// the workspace (an inspection boot of `ws.img`) and the task continues to SUCCEEDED. The
/// same with `ws.img` changed behind the journal's back fails the task as "workspace lost".
#[tokio::test(flavor = "multi_thread")]
async fn current_workspace_on_resume_boots_an_inspector_and_the_task_continues() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    for tampered in [false, true] {
        let env = Env::new(10);
        let root = kvm.root();
        let _guard = HomeGuard::new(root.path(), &kvm.cgroup_root);
        copy_dir(
            &env.dir.path().join("snapshot"),
            &root.path().join("snapshot"),
        );
        copy_dir(
            &env.dir.path().join("profile"),
            &root.path().join("profile"),
        );
        let worker = WorkerConfig::Firecracker(kvm.jailed_config(root.path()));
        let jobs = root.path().join("jobs");
        let hook = CrashHook::at(CrashPoint::AfterComplete, "read_snapshot");
        let dying = supervised(
            &jobs,
            worker.clone(),
            &ExecCounts::default(),
            Some(hook.clone()),
            &[],
        );
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let err = run_task_with(
            &env.db,
            &env.blobs,
            &dying,
            &mut agent,
            &env.task,
            &RunOptions::crash_with(hook),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, EngineError::Crashed(CrashPoint::AfterComplete)),
            "{err:?}"
        );
        let ws_img = root
            .path()
            .join("work")
            .join(env.task.as_str())
            .join("ws.img");
        if tampered {
            fs::write(root.path().join("stray.py"), b"x = 1\n").unwrap();
            common::debugfs_write(
                &ws_img,
                &format!(
                    "cd /src\nwrite {} stray.py\n",
                    root.path().join("stray.py").display()
                ),
            );
        }
        // Every Firecracker of this home while the task resumes.
        let home = root.path().to_path_buf();
        let seen = sample(Duration::from_millis(20), move |_| {
            let ids: Vec<String> = home_firecrackers(&home)
                .iter()
                .filter_map(|p| p.id().map(str::to_string))
                .collect();
            (!ids.is_empty()).then_some(ids)
        });
        let exec = supervised(&jobs, worker, &ExecCounts::default(), None, &[]);
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
            .await
            .unwrap();
        let seen = seen.stop();
        let first = seen.first().cloned().unwrap_or_default();
        assert!(
            first.iter().all(|id| id.starts_with("inspect-")) && !first.is_empty(),
            "the first VM of the resume is an inspection: {seen:?}"
        );
        if tampered {
            assert_eq!(state, TaskState::Failed);
            let failed: Vec<serde_json::Value> = env
                .events()
                .into_iter()
                .filter(|e| e.event_type == "Failed")
                .map(|e| e.payload)
                .collect();
            let reason = failed
                .first()
                .and_then(|f| f["Failed"]["reason"].as_str())
                .unwrap_or_default()
                .to_string();
            assert!(
                reason.starts_with("workspace lost: expected "),
                "{failed:?} {:?}",
                env.event_types()
            );
        } else {
            assert_eq!(state, TaskState::Succeeded, "{:?}", env.event_types());
        }
        assert!(home_firecrackers(root.path()).is_empty());
    }
}

/// The controller dies while a verification's VM runs (its check sleeps); recovery waits
/// for the job past its (clamped) lease bound and fences it; the fenced job's kill receipt is
/// what recovery publishes (`WaitedForJob`), and the task then resumes with an inspection VM
/// of its own. With samples every 5 ms of this home's Firecracker processes (by `--id`): the
/// old attempt's VM is seen alive after recovery began, no sample ever holds the old VM and a
/// later one (the resume's inspection) together, and the home never runs more than two at once.
#[tokio::test(flavor = "multi_thread")]
async fn concurrency_bound_never_exceeds_two_firecracker_processes() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let env = Env::new(10);
    let root = kvm.root();
    let _guard = HomeGuard::new(root.path(), &kvm.cgroup_root);
    copy_dir(
        &env.dir.path().join("snapshot"),
        &root.path().join("snapshot"),
    );
    copy_dir(
        &env.dir.path().join("profile"),
        &root.path().join("profile"),
    );
    // Longer than recovery's wait (the 200 ms clamp plus the 5 s grace), short enough for
    // the re-dispatched attempt to finish the task.
    let script = serde_json::json!({ "id": "kvm", "command": ["python3", "-c", "import time; time.sleep(8)"], "protected": true });
    fs::write(root.path().join("profile/profile.json"), script.to_string()).unwrap();
    let worker = WorkerConfig::Firecracker(kvm.jailed_config(root.path()));
    let jobs = root.path().join("jobs");
    let started = Instant::now();
    let home = root.path().to_path_buf();
    let seen = sample(Duration::from_millis(5), move |at| {
        let ids: Vec<String> = home_firecrackers(&home)
            .iter()
            .filter_map(|p| p.id().map(str::to_string))
            .collect();
        Some((at, ids))
    });
    let hook = CrashHook::at(CrashPoint::DuringExecute, "run_verification");
    let dying = supervised(
        &jobs,
        worker.clone(),
        &ExecCounts::default(),
        Some(hook.clone()),
        &[],
    );
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let err = run_task_with(
        &env.db,
        &env.blobs,
        &dying,
        &mut agent,
        &env.task,
        &RunOptions::crash_with(hook),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, EngineError::Crashed(CrashPoint::DuringExecute)),
        "{err:?}"
    );
    let verify = env.effects("RunVerification").remove(0);
    let old = JobDir::list(&jobs, &verify.effect_id)
        .unwrap()
        .pop()
        .unwrap()
        .request()
        .unwrap()
        .attempt_id
        .to_string();
    wait_for_async("the old attempt's VM", || {
        firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(old.as_str()))
    })
    .await;

    let recover_began = started.elapsed();
    let exec = supervised(&jobs, worker, &ExecCounts::default(), None, &[])
        .with_max_lease_clamp(Duration::from_millis(200));
    let report = recover(&env.db, &env.blobs, &exec, &env.task)
        .await
        .unwrap();
    // The fence stopped the old job; its kill receipt is what recovery publishes.
    let decisions: Vec<Decision> = report
        .decisions
        .iter()
        .filter(|d| d.effect_id == verify.effect_id)
        .map(|d| d.decision)
        .collect();
    assert_eq!(
        decisions.first(),
        Some(&Decision::WaitedForJob),
        "{report:?}"
    );
    // The task goes on: the resume asks the inspector for the workspace (a VM of its own).
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let finished = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
        .await
        .unwrap();
    // The fenced verification's kill receipt is a failed check, so the resumed run ends Failed.
    assert_eq!(finished, TaskState::Failed);
    let seen = seen.stop();

    let others = |ids: &[String]| ids.iter().any(|i| *i != old);
    let has_old = |ids: &[String]| ids.contains(&old);
    // (a) Not vacuous: the old VM was alive while recovery ran, and another VM of this home
    // ran after recovery began.
    let old_after = seen
        .iter()
        .filter(|(at, ids)| *at > recover_began && has_old(ids))
        .count();
    assert!(
        old_after > 0,
        "the old attempt's VM was never seen after recovery began"
    );
    let later: std::collections::BTreeSet<&String> = seen
        .iter()
        .filter(|(at, _)| *at > recover_began)
        .flat_map(|(_, ids)| ids.iter().filter(|i| **i != old))
        .collect();
    assert!(
        !later.is_empty(),
        "no other VM of this home ran after recovery began"
    );
    // (b) The fence stopped the old VM before any other VM of this home started.
    let both: Vec<(&Duration, &Vec<String>)> = seen
        .iter()
        .filter(|(_, ids)| has_old(ids) && others(ids))
        .map(|(at, ids)| (at, ids))
        .collect();
    assert!(
        both.is_empty(),
        "the old attempt's VM ran together with another: {both:?}"
    );
    // (c) The bound.
    let max = seen.iter().map(|(_, ids)| ids.len()).max().unwrap_or(0);
    println!(
        "concurrency: {} samples, old VM seen {old_after} times after recovery began, later VMs {later:?}, at most {max} at once",
        seen.len()
    );
    assert!(max <= 2, "{max} Firecracker processes at once");
}

/// `HomeGuard` (the cleanup every test of this tier relies on when an assertion fails):
/// dropped, it kills the home's Firecracker processes and collects their jails and cgroups.
/// The "VM" is a stand-in: a copy of the Python interpreter named `firecracker`, sleeping
/// with `--id <attempt>` in its command line, inside a real `agentos/<attempt>` cgroup.
#[tokio::test(flavor = "multi_thread")]
async fn home_guard_kills_the_homes_vms_and_collects_their_jails() {
    let Some(kvm) = kvm::require() else { return };
    let root = kvm.root();
    let id = AttemptId::new().to_string();
    let job = root
        .path()
        .join("jobs")
        .join(format!("{}-{id}", "e".repeat(64)));
    let cgroup = kvm.cgroup_root.join("agentos").join(&id);
    fs::create_dir_all(job.join("jail/firecracker").join(&id).join("root")).unwrap();
    fs::create_dir_all(&cgroup).unwrap();
    fs::write(job.join("jail/cgroup"), format!("{}\n", cgroup.display())).unwrap();
    let stand_in = root.path().join("firecracker");
    fs::copy(fs::canonicalize("/usr/bin/python3").unwrap(), &stand_in).unwrap();
    let mut cmd = Command::new(&stand_in);
    cmd.args(["-c", "import time; time.sleep(60)", "--id", id.as_str()])
        .env("PYTHONHOME", "/usr");
    // The copy was just written: a fork by another test thread can briefly hold its write fd
    // (until that child's exec closes it), and exec then fails with ETXTBSY. Retry that only.
    let mut busy = 0;
    let mut vm = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(26) && busy < 100 => {
                busy += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("spawn the stand-in {}: {e}", stand_in.display()),
        }
    };
    fs::write(cgroup.join("cgroup.procs"), vm.id().to_string()).unwrap();
    wait_for("the stand-in", || {
        firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(id.as_str()))
    });

    drop(HomeGuard::new(root.path(), &kvm.cgroup_root));
    assert!(
        !firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(id.as_str())),
        "the guard killed the VM"
    );
    assert!(!cgroup.exists(), "and removed its cgroup");
    assert!(!job.join("jail").exists(), "and its jail");
    let status = vm.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
}

// ---------------------------------------------------------------------------------------
// The jail.

#[tokio::test(flavor = "multi_thread")]
async fn jailed_firecracker_runs_as_the_jail_uid_in_its_chroot_and_cgroup() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script("import time; time.sleep(3); print('PASSED')");
    let req = fx.request(EffectKind::RunVerification, b"");
    let c = ctx(2);
    let job = JobDir::create(&fx.path("jobs"), &fx.job_request(&req, &c, &fx.cfg, None))
        .unwrap()
        .0;
    let worker = FirecrackerWorker::new(&fx.cfg, &job).with_env(test_env());
    let running = {
        let (req, c) = (req.clone(), c.clone());
        tokio::spawn(async move { worker.run(&req, &c).await })
    };
    let id = c.attempt_id.to_string();
    let chroot = job.path.join("jail/firecracker").join(&id).join("root");
    wait_for("the VM's socket", || chroot.join("v.sock").exists());
    let procs: Vec<FcProc> = firecracker_processes()
        .into_iter()
        .filter(|p| p.id() == Some(id.as_str()))
        .collect();
    assert_eq!(procs.len(), 1, "{procs:?}");
    let fc = &procs[0];
    assert_eq!(fc.uid, JAIL_UID, "{fc:?}");
    assert_eq!(
        &fc.cmdline[..3],
        ["/firecracker", "--id", id.as_str()],
        "{fc:?}"
    );
    assert_eq!(
        &fc.cmdline[fc.cmdline.len() - 3..],
        ["--no-api", "--config-file", "/vm.json"],
        "{fc:?}"
    );
    assert!(
        !fc.cmdline
            .iter()
            .any(|a| a.contains(fx.root().to_str().unwrap())),
        "no host path: {fc:?}"
    );
    assert_eq!(fc.cgroup, format!("0::/agentos/{id}"));
    let procs_file = fs::read_to_string(
        kvm.cgroup_root
            .join("agentos")
            .join(&id)
            .join("cgroup.procs"),
    )
    .unwrap();
    assert_eq!(
        procs_file.split_whitespace().collect::<Vec<_>>(),
        [fc.pid.to_string()]
    );
    let mut names: Vec<String> = fs::read_dir(&chroot)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    names.sort();
    // The six staged files, the jailer's copy of the binary, its pid file, `dev/` (the
    // jailer's device nodes, `kvm` among them), `run/` (empty: no API socket with
    // `--no-api`), and the VM's socket.
    assert_eq!(
        names,
        [
            "dev",
            "firecracker",
            "firecracker.log",
            "firecracker.pid",
            "rootfs.squashfs",
            "run",
            "scratch.img",
            "v.sock",
            "vm.json",
            "vmlinux",
            "ws.img"
        ],
        "the chroot"
    );
    let run: Vec<_> = fs::read_dir(chroot.join("run"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(run.is_empty(), "run/ holds {run:?}");
    assert!(chroot.join("dev/kvm").exists());
    assert_eq!(
        fs::read_to_string(chroot.join("firecracker.pid"))
            .unwrap()
            .trim(),
        fc.pid.to_string()
    );
    // The socket is `v.sock` relative to the chroot root (vm.json's relative `uds_path`):
    // inside the jail, none beside the job.
    assert!(!job.path.join("v.sock").exists());
    let out = running.await.unwrap();
    assert_eq!(evidence(&out)["passed"], true);
    assert!(
        !job.path.join("jail").exists() && !fx.cgroup_of(&c).exists(),
        "collected after the run"
    );
    assert_eq!(
        nlink(&fx.ws_img()),
        1,
        "the chroot's link to ws.img is gone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jail_cgroup_limits_equal_the_contract() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/mem-hog");
    let (ran, samples) = fx.verify_watched(Duration::from_millis(100), |w| w).await;
    assert_eq!(
        evidence(&ran.out)["exit_code"],
        serde_json::Value::Null,
        "mem-hog was OOM-killed in the guest"
    );
    // Samples taken once Firecracker runs: the jailer writes the limits before its `exec`
    // (a sample of the cgroup alone may catch it between `mkdir` and the writes).
    let with_limits: Vec<&VmSample> = samples
        .iter()
        .filter(|s| !s.procs.is_empty() && s.cgroup.contains_key("cpu.max"))
        .collect();
    assert!(
        !with_limits.is_empty(),
        "the cgroup was read while the VM ran: {samples:?}"
    );
    for s in &with_limits {
        let read = |f: &str| s.cgroup[f].trim().to_string();
        assert_eq!(read("cpu.max"), "100000 100000");
        assert_eq!(read("memory.max"), ((256u64 + 128) * 1048576).to_string());
        assert_eq!(read("memory.swap.max"), "0");
        assert_eq!(read("pids.max"), "64");
        assert_eq!(
            keyed(&s.cgroup["memory.events"]).get("oom_kill"),
            Some(&0),
            "{:?}",
            s.cgroup["memory.events"]
        );
        if let Some(rss) = s.rss_kib {
            assert!(rss <= (256 + 96) * 1024, "VmRSS {rss} KiB");
        }
    }
}

/// `memory.max` lowered to 96 MiB under a 256 MiB guest running mem-hog: the cgroup's OOM
/// killer ends Firecracker itself (SIGKILL), which the worker reports; the next VM boots.
#[tokio::test(flavor = "multi_thread")]
async fn memory_hog_firecracker_is_oom_killed_by_the_cgroup_when_the_bound_is_lowered() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/mem-hog");
    let req = fx.request(EffectKind::RunVerification, b"");
    let c = ctx(2);
    let job = JobDir::create(&fx.path("jobs"), &fx.job_request(&req, &c, &fx.cfg, None))
        .unwrap()
        .0;
    let worker = FirecrackerWorker::new(&fx.cfg, &job)
        .with_env(test_env())
        .with_jail_memory_max_mib(96);
    // This VM's own cgroup, read as often as it can be: the window between the kill and the
    // worker's collection of the cgroup is a few milliseconds. (The parent `agentos/` counts
    // every test's kills, so it proves nothing about this one.)
    let events = fx.cgroup_of(&c).join("memory.events");
    let own = sample(Duration::from_micros(200), move |_| {
        fs::read_to_string(&events)
            .ok()
            .and_then(|e| keyed(&e).get("oom_kill").copied())
    });
    let watch = watch_vm(
        c.attempt_id.to_string(),
        kvm.cgroup_root.clone(),
        Duration::from_millis(5),
    );
    let out = worker.run(&req, &c).await;
    let (own, samples) = (own.stop(), watch.stop());
    let ran = Ran {
        out,
        job,
        ctx: c,
        took: Duration::ZERO,
    };
    assert_eq!(
        failure(&ran.out),
        "guest exited before reporting: firecracker killed by signal 9"
    );
    let seen_in_own = own.iter().copied().max().unwrap_or(0);
    println!(
        "oom_kill: own cgroup {seen_in_own} over {} reads",
        own.len()
    );
    assert!(
        samples.iter().any(|s| s
            .cgroup
            .get("memory.max")
            .is_some_and(|m| m.trim() == (96u64 << 20).to_string())),
        "the lowered bound applied"
    );
    assert!(
        seen_in_own >= 1,
        "the VM's own cgroup recorded the OOM kill ({} reads)",
        own.len()
    );
    assert!(!fx.cgroup_of(&ran.ctx).exists(), "collected");
    assert!(home_firecrackers(fx.root()).is_empty());
    // The next effect's VM boots (with the contract's bound).
    fx.use_profile("parser-checks-v1");
    let next = fx.run(EffectKind::RunVerification).await;
    assert_eq!(
        evidence(&next.out)["exit_code"],
        1,
        "the unpatched fixture fails its check, in a VM that came up"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_burn_is_throttled_by_the_cgroup_when_the_quota_is_lowered() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/cpu-burn");
    let (ran, samples) = fx
        .verify_watched(Duration::from_millis(50), |w| {
            w.with_jail_cpu_quota_us(50_000)
        })
        .await;
    let v = evidence(&ran.out);
    assert_eq!(
        (v["passed"].clone(), findings(&v)["nproc"].clone()),
        (true.into(), 1.into()),
        "the check still completes: {v}"
    );
    let stats: Vec<(Duration, BTreeMap<String, u64>)> = samples
        .iter()
        .filter_map(|s| Some((s.at, keyed(s.cgroup.get("cpu.stat")?))))
        .collect();
    assert!(
        samples.iter().any(|s| s
            .cgroup
            .get("cpu.max")
            .is_some_and(|m| m.trim() == "50000 100000")),
        "the lowered quota applied"
    );
    let (first, (last_at, last)) = (stats[0].0, stats.last().unwrap().clone());
    // From the cgroup's first sight to its last: the VM's whole life within a sample.
    let wall = (last_at - first).as_micros() as f64 + 50_000.0;
    let usage = last["usage_usec"] as f64;
    println!(
        "throttled: usage {usage} µs over {wall} µs, nr_throttled {}",
        last["nr_throttled"]
    );
    assert!(last["nr_throttled"] > 0, "{last:?}");
    assert!(usage <= 0.6 * wall, "usage {usage} µs over {wall} µs");
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_bomb_never_adds_a_host_process() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_profile("hostile/fork-bomb");
    let (ran, samples) = fx.verify_watched(Duration::from_millis(50), |w| w).await;
    assert!(
        !matches!(ran.out.receipt.outcome, Outcome::Success)
            || evidence(&ran.out)["passed"] == false
    );
    let pids: Vec<u64> = samples
        .iter()
        .filter_map(|s| s.cgroup.get("pids.current")?.trim().parse().ok())
        .collect();
    assert!(pids.len() >= 20, "sampled through the bomb: {}", pids.len());
    let most = pids.iter().copied().max().unwrap();
    println!(
        "fork-bomb: pids.current at most {most} over {} samples",
        pids.len()
    );
    assert!(most <= MAX_FIRECRACKER_TASKS, "pids.current {most}");
    assert!(
        samples.iter().all(|s| s.procs.len() <= 1),
        "one Firecracker for the attempt"
    );
}

/// From the first jailed `ReadSnapshot` on, `ws.img` is the jail's (0600, uid 61000); after
/// the collection it has one link again, and the controller (root) still inspects it.
#[tokio::test(flavor = "multi_thread")]
async fn staged_ws_img_is_owned_by_the_jail_uid_and_still_inspectable() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    let ran = fx.snapshot().await;
    let meta = fs::metadata(fx.ws_img()).unwrap();
    assert_eq!(
        (meta.uid(), meta.gid(), meta.mode() & 0o7777, meta.nlink()),
        (JAIL_UID, JAIL_UID, 0o600, 1)
    );
    assert_eq!(
        Some(fx.digest()),
        ran.out.new_workspace,
        "the inspector reads the jail's image"
    );
    assert_eq!(
        fs::metadata(fx.ws_img()).unwrap().nlink(),
        1,
        "the inspection's link is collected too"
    );
}

/// SIGKILL the supervisor during a verification, then everything of the job dies: the jail
/// and the empty cgroup are left (the chroot's hard link keeps `scratch.img`), until the
/// controller's fence on resume collects them.
#[tokio::test(flavor = "multi_thread")]
async fn leftover_jail_after_supervisor_sigkill_is_collected_on_resume() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script("import time; time.sleep(30)");
    let req = fx.request(EffectKind::RunVerification, b"");
    let c = ctx(2);
    let dying = fx.executor(
        Some(CrashHook::at(CrashPoint::DuringExecute, "run_verification")),
        &[],
    );
    let out = dying.run(&req, &c).await;
    assert_eq!(failure(&out), "injected crash after the launch");
    let job = JobDir::list(&fx.path("jobs"), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    let id = c.attempt_id.to_string();
    wait_for_async("the VM", || {
        firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(id.as_str()))
    })
    .await;
    wait_for_async("Running status", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Running)
    })
    .await;
    let status = job.read_status().unwrap();
    let supervisor = Pid::from_raw(status.supervisor_pid.unwrap() as i32).unwrap();
    kill_process(supervisor, Signal::KILL).unwrap();
    wait_for_async("the job's lock to be free", || job.is_dead()).await;
    // The worker and its VM outlive the supervisor; then they die too (the whole group, as
    // a host crash would take them), leaving the jail as it was.
    let vm_pid = firecracker_processes()
        .into_iter()
        .find(|p| p.id() == Some(id.as_str()))
        .unwrap()
        .pid;
    let _ = kill_process_group(
        Pid::from_raw(status.worker_pgid.unwrap()).unwrap(),
        Signal::KILL,
    );
    wait_for_async("the VM to die", || !alive(vm_pid)).await;
    let (cgroup, jail, scratch) = (
        fx.cgroup_of(&c),
        job.path.join("jail"),
        job.path.join("scratch.img"),
    );
    assert!(cgroup.is_dir(), "the cgroup is left: {}", cgroup.display());
    assert_eq!(
        fs::read_to_string(cgroup.join("cgroup.procs"))
            .unwrap()
            .trim(),
        "",
        "and empty"
    );
    assert!(jail.is_dir(), "the jail is left");
    assert_eq!(nlink(&scratch), 2, "scratch.img and its link in the chroot");

    let controller = fx.executor(None, &[]);
    assert!(
        controller.fence_jobs(std::slice::from_ref(&job)).await,
        "the fence settles the dead job"
    );
    assert!(!cgroup.exists(), "the cgroup is collected");
    assert!(!jail.exists(), "the jail is collected");
    assert_eq!(nlink(&scratch), 1);
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

/// SIGKILL the supervisor during a verification while the VM lives on: the controller's
/// fence kills the job's worker and VM itself and collects the jail and the real cgroup
/// right after (the `rmdir` right after a kill on a real cgroup).
#[tokio::test(flavor = "multi_thread")]
async fn the_fence_kills_a_live_jailed_vm_and_collects_its_cgroup() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script("import time; time.sleep(30)");
    let req = fx.request(EffectKind::RunVerification, b"");
    let c = ctx(2);
    let dying = fx.executor(
        Some(CrashHook::at(CrashPoint::DuringExecute, "run_verification")),
        &[],
    );
    assert_eq!(
        failure(&dying.run(&req, &c).await),
        "injected crash after the launch"
    );
    let job = JobDir::list(&fx.path("jobs"), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    let id = c.attempt_id.to_string();
    wait_for_async("the VM", || {
        firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(id.as_str()))
    })
    .await;
    wait_for_async("Running status", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Running)
    })
    .await;
    let supervisor =
        Pid::from_raw(job.read_status().unwrap().supervisor_pid.unwrap() as i32).unwrap();
    kill_process(supervisor, Signal::KILL).unwrap();
    wait_for_async("the job's lock to be free", || job.is_dead()).await;
    let vm = firecracker_processes()
        .into_iter()
        .find(|p| p.id() == Some(id.as_str()))
        .expect("the VM outlives the supervisor");
    let cgroup = fx.cgroup_of(&c);
    assert_eq!(
        fs::read_to_string(cgroup.join("cgroup.procs"))
            .unwrap()
            .trim(),
        vm.pid.to_string()
    );

    let controller = fx.executor(None, &[]);
    assert!(
        controller.fence_jobs(std::slice::from_ref(&job)).await,
        "the fence settles the job"
    );
    assert!(!alive(vm.pid), "the fence killed the VM");
    assert!(
        !cgroup.exists(),
        "the cgroup is collected right after the kill"
    );
    assert!(!job.path.join("jail").exists(), "the jail is collected");
    assert_eq!(nlink(&job.path.join("scratch.img")), 1);
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

/// The cgroup seams count only in a test run: without `AGENTOS_TEST_WORKERS=1` a lowered
/// `memory.max` is ignored and the jail gets the contract's.
#[tokio::test(flavor = "multi_thread")]
async fn the_cgroup_seams_are_ignored_without_the_test_switch() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    fx.snapshot().await;
    fx.use_script("import time; time.sleep(1); print('PASSED')");
    let (ran, samples) = fx
        .verify_watched(Duration::from_millis(50), |w| {
            w.with_env(Vec::new())
                .with_jail_memory_max_mib(96)
                .with_jail_cpu_quota_us(10_000)
        })
        .await;
    assert_eq!(evidence(&ran.out)["passed"], true);
    // Once Firecracker runs (the jailer has written every limit by its `exec`).
    let seen: Vec<(&str, &str)> = samples
        .iter()
        .filter(|s| !s.procs.is_empty())
        .filter_map(|s| {
            Some((
                s.cgroup.get("memory.max")?.trim(),
                s.cgroup.get("cpu.max")?.trim(),
            ))
        })
        .collect();
    assert!(!seen.is_empty(), "the cgroup was read");
    let contract = ((256u64 + 128) * 1048576).to_string();
    assert!(
        seen.iter()
            .all(|(m, c)| *m == contract && *c == "100000 100000"),
        "{seen:?}"
    );
}

/// Set only in the child of `a_dead_inspectors_jail_is_collected_by_the_next_inspection`:
/// the JSON config file it inspects with.
const INSPECT_CHILD: &str = "AGENTOS_KVM_INSPECT_CHILD";

/// Not a test of its own: in a child, one inspection, which its parent kills mid-way.
#[test]
fn inspect_child_entry() {
    let Some(config) = std::env::var_os(INSPECT_CHILD) else {
        return;
    };
    let (cfg, inspect_root, task): (FirecrackerConfig, PathBuf, TaskId) =
        serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
    let _ = Inspector::new(cfg, inspect_root)
        .with_env(test_env())
        .query(&task, Query::Digest);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_inspectors_jail_is_collected_by_the_next_inspection() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    let ran = fx.snapshot().await;
    let config = fx.path("inspect-child.json");
    fs::write(
        &config,
        serde_json::to_vec(&(&fx.cfg, fx.path("inspect"), &fx.task)).unwrap(),
    )
    .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inspect_child_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INSPECT_CHILD, &config)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let task_root = fx.path("inspect").join(fx.task.as_str());
    let mut dead_dir = None;
    wait_for("the child's inspection VM", || {
        let vms = home_firecrackers(fx.root());
        dead_dir = vms
            .iter()
            .find_map(|p| p.id()?.strip_prefix("inspect-").map(|u| task_root.join(u)));
        dead_dir.is_some()
    });
    child.kill().unwrap();
    child.wait().unwrap();
    let dead_dir = dead_dir.unwrap();
    let uuid = dead_dir.file_name().unwrap().to_str().unwrap().to_string();
    let cgroup = kvm
        .cgroup_root
        .join("agentos")
        .join(format!("inspect-{uuid}"));
    // The orphaned VM powers itself off once its connection is gone (or never came).
    wait_for("the orphaned inspection VM to end", || {
        !firecracker_processes()
            .iter()
            .any(|p| p.id() == Some(&format!("inspect-{uuid}")))
    });
    assert!(
        dead_dir.is_dir() && cgroup.is_dir(),
        "the dead inspector left its directory and cgroup"
    );

    assert_eq!(
        Some(fx.digest()),
        ran.out.new_workspace,
        "the next inspection answers"
    );
    assert!(
        !dead_dir.exists(),
        "it removed the dead inspector's directory"
    );
    assert!(!cgroup.exists(), "and its cgroup");
    assert_eq!(fs::read_dir(&task_root).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_unjailed_runs_firecracker_as_the_container_user_without_a_cgroup() {
    let Some(kvm) = kvm::require() else { return };
    let _vm = shared().await;
    let fx = Fx::new(&kvm);
    let cfg = kvm.unjailed_config(fx.root());
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let c = ctx(1);
    let job = JobDir::create(&fx.path("jobs"), &fx.job_request(&req, &c, &cfg, None))
        .unwrap()
        .0;
    let worker = FirecrackerWorker::new(&cfg, &job).with_env(test_env());
    let running = {
        let (req, c) = (req.clone(), c.clone());
        tokio::spawn(async move { worker.run(&req, &c).await })
    };
    let id = c.attempt_id.to_string();
    // The relative `uds_path` ("v.sock") binds in Firecracker's working directory, the job's.
    wait_for("the VM's socket in the job directory", || {
        job.path.join("v.sock").exists()
    });
    let fc = firecracker_processes()
        .into_iter()
        .find(|p| p.id() == Some(id.as_str()))
        .expect("the VM");
    let own_cgroup = fs::read_to_string("/proc/self/cgroup")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("0::"))
        .unwrap()
        .to_string();
    assert_eq!(fc.uid, rustix::process::getuid().as_raw(), "{fc:?}");
    assert_eq!(fc.cgroup, own_cgroup, "the test's own cgroup, no jail's");
    assert!(
        fc.cmdline
            .iter()
            .any(|a| a == &job.path.join("vm.json").display().to_string()),
        "{fc:?}"
    );
    assert!(!job.path.join("jail").exists(), "no jail");
    assert!(!fx.cgroup_of(&c).exists(), "no cgroup");
    let out = running.await.unwrap();
    assert_eq!(
        out.new_workspace,
        Some(workspace_digest(&fx.path("snapshot")).unwrap()),
        "{}",
        out_text(&out)
    );
    assert_eq!(
        fs::metadata(fx.ws_img()).unwrap().uid(),
        rustix::process::getuid().as_raw(),
        "ws.img stays the user's"
    );
}

/// `scripts/build-guest-image.sh … --verify` builds the image twice and compares the bytes.
/// Slow (two image builds, network for the Debian snapshot): `-- --ignored`.
#[test]
#[ignore = "slow: image build"]
fn image_build_is_reproducible() {
    let Some(_kvm) = kvm::require() else { return };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    // A fresh output directory, never $AGENTOS_GUEST_IMAGE: --verify replaces OUT_DIR.
    let out = tempfile::tempdir().unwrap();
    let image = out.path().join("python-stdlib-v1");
    let run = Command::new("sh")
        .arg(repo.join("scripts/build-guest-image.sh"))
        .arg("guest/python-stdlib-v1")
        .arg(&image)
        .arg("--verify")
        .current_dir(&repo)
        .output()
        .unwrap();
    let stdout = text(&run.stdout);
    assert!(run.status.success(), "{stdout}\n{}", text(&run.stderr));
    println!(
        "{}{stdout}",
        text(&run.stderr)
            .lines()
            .filter(|l| l.contains("--verify"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    );
    assert!(
        text(&run.stderr).contains("--verify: two builds are byte-identical")
            || stdout.contains("byte-identical"),
        "{stdout}"
    );
    for f in kvm::IMAGE_FILES {
        assert!(image.join(f).is_file(), "{f}");
    }
}
