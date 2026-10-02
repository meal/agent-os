//! The supervised executor against the real `agentos-supervisor` binary.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectState, Outcome};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::lease::EffectTimeouts;
use agentos_core::state::TaskState;
use agentos_engine::agent::{AgentAction, Observation};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::job::{HostConfig, JobDir, JobRequest, JobState, JobStatus, KillReason, ScriptedConfig, WorkerConfig};
use agentos_engine::runner::run_task;
use agentos_engine::supervised::{ExecCounts, JobWait, SupervisedExecutor};
use agentos_engine::supervisor::SupervisorCmd;
use agentos_engine::workspace::workspace_digest;
use common::{contract, copy_dir, edit_patch, fix_patch, fixtures, Env, FnAgent};
use rustix::process::{kill_process, Pid, Signal};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agentos-supervisor");
const TEST_WORKERS: &str = "AGENTOS_TEST_WORKERS";
const EXIT_BEFORE_RECEIPT: &str = "AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT";
/// Upper bound for anything a test waits on; every test should finish well inside it.
const PATIENCE: Duration = Duration::from_secs(10);

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn supervisor_cmd() -> SupervisorCmd {
    SupervisorCmd { program: BIN.into(), prefix_args: Vec::new() }
}

fn host_config(root: &std::path::Path) -> HostConfig {
    HostConfig {
        snapshot_dir: root.join("snapshot"),
        profile_dir: root.join("profile"),
        work_root: root.join("work"),
        verify_timeout_secs: 60,
        profile_digest: None,
    }
}

struct Fx {
    dir: TempDir,
    task: TaskId,
    contract: Contract,
    counts: ExecCounts,
}

impl Fx {
    fn new() -> Fx {
        Fx::for_task(TaskId::new())
    }

    /// With the parser fixture's snapshot and verification profile.
    fn for_task(task: TaskId) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        Fx { dir, task, contract: contract(10).0, counts: ExecCounts::default() }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn executor(&self, worker: WorkerConfig) -> SupervisedExecutor {
        SupervisedExecutor::new(self.path("jobs"), supervisor_cmd(), worker, self.counts.clone())
            .unwrap()
            .with_env(TEST_WORKERS, "1")
    }

    fn host(&self) -> SupervisedExecutor {
        self.executor(WorkerConfig::Host(host_config(self.dir.path())))
    }

    /// A host executor whose supervisor exits after the worker's outcome, before the receipt.
    fn hooked(&self) -> SupervisedExecutor {
        self.host().with_env(EXIT_BEFORE_RECEIPT, "1")
    }

    fn scripted(&self, script: &str) -> SupervisedExecutor {
        self.executor(WorkerConfig::Scripted(ScriptedConfig { script: script.into() }))
    }

    fn fixture(&self) -> FixtureExecutor {
        FixtureExecutor::new(self.path("snapshot"), self.path("profile"), self.path("work"))
    }

    fn ws(&self) -> PathBuf {
        self.fixture().workspace(&self.task)
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

    /// Points the profile at a Python script (on the copy; the fixture is untouched).
    fn script_profile(&self, script: &str) {
        let profile = serde_json::json!({ "id": "slow", "command": ["python3", "-c", script], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    fn jobs(&self, effect: &EffectId) -> Vec<JobDir> {
        JobDir::list(&self.path("jobs"), effect)
    }

    fn job(&self, effect: &EffectId) -> JobDir {
        let mut jobs = self.jobs(effect);
        assert_eq!(jobs.len(), 1, "one job for {effect}");
        jobs.remove(0)
    }

    async fn snapshot(&self) {
        let out = self.host().run(&self.request(EffectKind::ReadSnapshot, b""), &ctx(1)).await;
        assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
        assert_eq!(out.new_workspace, Some(self.base()));
    }
}

fn ctx(lease: u64) -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: lease, worker: "test".into() }
}

fn failure_reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!("expected a failure, got success: {}", String::from_utf8_lossy(&out.output)),
    }
}

/// The outcome is for this very attempt and describes its own output.
fn assert_for(out: &ExecOutcome, req: &EffectRequest, ctx: &AttemptCtx) {
    assert_eq!(out.receipt.effect_id, req.effect_id);
    assert_eq!(out.receipt.attempt_id, ctx.attempt_id);
    assert_eq!(out.receipt.lease_generation, ctx.lease_generation);
    assert_eq!(out.receipt.result_digest, Some(Digest::of(&out.output)));
}

/// SIGSTOPs a process and SIGKILLs it when dropped, so a failing test never leaves a
/// stopped supervisor behind.
struct Stopped(Pid);

impl Stopped {
    fn stop(pid: i32) -> Stopped {
        let pid = Pid::from_raw(pid).unwrap();
        kill_process(pid, Signal::STOP).unwrap();
        Stopped(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = kill_process(self.0, Signal::KILL);
    }
}

/// Kills and reaps a helper process when dropped.
struct Helper(std::process::Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A job whose lock the test holds, with a `status.json` naming `supervisor_pid` and
/// `worker_pgid`: what a worker that forged its job's status would leave.
fn forged_job(fx: &Fx, kind: EffectKind, supervisor_pid: u32, worker_pgid: i32) -> (JobDir, fs::File, EffectId) {
    let req = fx.request(kind, b"forged");
    let job_req = JobRequest {
        effect_id: req.effect_id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        payload: Vec::new(),
        contract: req.contract.clone(),
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        lease_expiry_ms: now_ms() + 3_600_000,
        task_deadline_ms: 0,
        worker: WorkerConfig::Scripted(ScriptedConfig { script: "true".into() }),
    };
    let (job, lock) = JobDir::create(&fx.path("jobs"), &job_req).unwrap();
    job.write_status(&JobStatus {
        state: JobState::Running,
        reason: None,
        supervisor_pid: Some(supervisor_pid),
        worker_pgid: Some(worker_pgid),
        updated_ms: now_ms(),
    })
    .unwrap();
    (job, lock, req.effect_id)
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A process is gone when `/proc` no longer has it or it is a zombie (nobody reaps orphans
/// in the test container).
fn gone(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_whitespace().next() == Some("Z")
}

#[tokio::test]
async fn run_matches_fixture_executor_for_all_three_kinds() {
    let task = TaskId::new();
    let (plain, supervised) = (Fx::for_task(task.clone()), Fx::for_task(task));
    let (fixture, exec) = (plain.fixture(), supervised.host());
    let base = plain.base();
    let requests = [
        plain.request(EffectKind::ReadSnapshot, b""),
        plain.request(EffectKind::ApplyPatch { expected_base: base }, fix_patch().as_bytes()),
        plain.request(EffectKind::RunVerification, b""),
    ];
    for (i, req) in requests.iter().enumerate() {
        let ctx = ctx(i as u64 + 1);
        let expected = fixture.run(req, &ctx).await;
        let actual = exec.run(req, &ctx).await;
        assert_eq!(expected.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&expected.output));
        assert_eq!(actual, expected, "{}", req.kind.tag());
        assert!(!actual.unresolved);
    }
    assert!(exec.retained_outcome(&requests[2].effect_id).unwrap().verification.unwrap().passed);
    assert_eq!(workspace_digest(&supervised.ws()).unwrap(), workspace_digest(&plain.ws()).unwrap());
}

#[tokio::test]
async fn launch_is_refused_when_the_deadline_has_passed_and_creates_no_job_dir() {
    let fx = Fx::new();
    let exec = fx.scripted("echo never");
    let now_secs = now_ms() / 1000;
    for deadline_ts in [now_secs - 1, now_secs - 3600, 1] {
        let req = EffectRequest { deadline_ts, ..fx.request(EffectKind::ReadSnapshot, b"") };
        let ctx = ctx(1);
        let out = exec.run(&req, &ctx).await;
        assert_eq!(failure_reason(&out), "deadline exceeded");
        assert_for(&out, &req, &ctx);
    }
    let left = fs::read_dir(fx.path("jobs")).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "no job directory was created");
    // A deadline in the future launches.
    let req = EffectRequest { deadline_ts: now_secs + 600, ..fx.request(EffectKind::ReadSnapshot, b"") };
    let out = exec.run(&req, &ctx(1)).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
    assert_eq!(out.output, b"never\n");
}

#[tokio::test]
async fn retained_outcome_finds_the_highest_lease_receipt() {
    let fx = Fx::new();
    let exec = fx.scripted("echo $$");
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    assert_eq!(exec.retained_outcome(&req.effect_id), None);
    let (high, low) = (ctx(3), ctx(1));
    // The higher lease runs first, so "the latest job" would be the wrong answer.
    let first = exec.run(&req, &high).await;
    let second = exec.run(&req, &low).await;
    assert_eq!(first.receipt.outcome, Outcome::Success);
    assert_ne!(first.output, second.output);
    let retained = exec.retained_outcome(&req.effect_id).unwrap();
    assert_eq!(retained, first);
    assert_eq!(retained.receipt.attempt_id, high.attempt_id);
    assert_eq!(fx.jobs(&req.effect_id).len(), 2);
    let other = fx.request(EffectKind::RunVerification, b"");
    assert_eq!(exec.retained_outcome(&other.effect_id), None);
}

#[tokio::test]
async fn a_new_executor_on_the_same_jobs_root_still_finds_receipts_of_jobs_the_old_one_launched() {
    let fx = Fx::new();
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let out = {
        let old = fx.host();
        old.run(&req, &ctx(1)).await
    };
    assert_eq!(out.receipt.outcome, Outcome::Success);
    let restarted = fx.host();
    assert_eq!(restarted.retained_outcome(&req.effect_id), Some(out));
}

#[tokio::test]
async fn killed_verification_returns_the_failure_receipt() {
    let fx = Fx::new();
    fx.snapshot().await;
    fx.script_profile("import time; time.sleep(30)");
    let timeouts = EffectTimeouts { verification: Duration::from_millis(800), other: Duration::from_secs(30) };
    let exec = fx.host().with_timeouts(timeouts);
    let (req, ctx) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let started = Instant::now();
    let out = exec.run(&req, &ctx).await;
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    assert_eq!(failure_reason(&out), "lease expired");
    assert_for(&out, &req, &ctx);
    assert!(!out.unresolved);
    let job = fx.job(&req.effect_id);
    let status = job.read_status().unwrap();
    assert_eq!((status.state, status.reason), (JobState::Killed, Some(KillReason::Lease)));
    assert_eq!(job.read_receipt(), Some(out));
}

#[tokio::test]
async fn killed_apply_patch_without_a_receipt_is_reconciled_not_failed() {
    let fx = Fx::new();
    fx.snapshot().await;
    let (req, ctx) = (fx.patch(&fix_patch()), ctx(1));
    let out = fx.hooked().run(&req, &ctx).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
    assert_for(&out, &req, &ctx);
    assert!(!out.unresolved);
    let digest = workspace_digest(&fx.ws()).unwrap();
    assert_ne!(digest, fx.base(), "the patch was applied");
    assert_eq!(out.new_workspace, Some(digest));
    // It came from reconciliation: the supervisor died before writing any receipt.
    let job = fx.job(&req.effect_id);
    assert_eq!(job.read_receipt(), None);
    assert!(job.read_outcome().is_some(), "the worker did finish");
    let Reconciliation::Applied(expected) = fx.fixture().reconcile(&req, &ctx).await else { panic!("not applied") };
    assert_eq!(out, expected);
}

#[tokio::test]
async fn killed_apply_patch_that_provably_did_not_apply_returns_a_truthful_failure() {
    let fx = Fx::new();
    fx.snapshot().await;
    // Valid patch text that does not apply to the workspace: the worker fails, and the
    // supervisor dies before turning that into a receipt.
    let (req, ctx) = (fx.patch(&edit_patch("src/parser.py", "no such line", "x")), ctx(1));
    let out = fx.hooked().run(&req, &ctx).await;
    assert_eq!(failure_reason(&out), "patch provably not applied");
    assert_for(&out, &req, &ctx);
    assert!(!out.unresolved);
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), fx.base());
    assert_eq!(fx.job(&req.effect_id).read_receipt(), None);
}

/// Snapshots go through a plain supervised executor, patches through one whose supervisor
/// exits before writing the receipt.
struct ByKind {
    plain: SupervisedExecutor,
    hooked: SupervisedExecutor,
}

impl Executor for ByKind {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::ApplyPatch { .. } => self.hooked.run(req, ctx).await,
            _ => self.plain.run(req, ctx).await,
        }
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.plain.retained_outcome(effect)
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        self.plain.reconcile(req, ctx).await
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.plain.current_workspace(task)
    }
}

#[tokio::test]
async fn unresolvable_apply_patch_returns_an_unresolved_outcome_and_the_runner_marks_it_unknown_and_fails_the_task() {
    // The executor: a workspace that is neither the base nor the base plus the patch.
    let fx = Fx::new();
    fx.snapshot().await;
    fs::write(fx.ws().join("src/stray.py"), "x = 1\n").unwrap();
    let (req, ctx) = (fx.patch(&fix_patch()), ctx(1));
    let out = fx.hooked().run(&req, &ctx).await;
    assert!(out.unresolved, "{}", String::from_utf8_lossy(&out.output));
    assert!(matches!(out.receipt.outcome, Outcome::Failure(_)));
    assert_for(&out, &req, &ctx);

    // The runner: the same situation inside a task.
    let env = Env::new(10);
    let jobs = env.dir.path().join("jobs");
    let make = || {
        SupervisedExecutor::new(jobs.clone(), supervisor_cmd(), WorkerConfig::Host(host_config(env.dir.path())), ExecCounts::default())
            .unwrap()
            .with_env(TEST_WORKERS, "1")
    };
    let exec = ByKind { plain: make(), hooked: make().with_env(EXIT_BEFORE_RECEIPT, "1") };
    let ws = env.ws();
    let mut agent = FnAgent(move |obs: &Observation| match obs {
        Observation::Start { .. } => {
            fs::write(ws.join("src/stray.py"), "x = 1\n").unwrap();
            AgentAction::ApplyPatch(fix_patch())
        }
        _ => AgentAction::Finish,
    });
    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task).await.unwrap();
    assert_eq!(state, TaskState::Failed);
    let patch = env.effects("ApplyPatch").remove(0);
    assert_eq!(patch.state, EffectState::Unknown);
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(usage.uncertain_tool_actions, 1, "{usage:?}");
    let failed: Vec<_> = env.events().into_iter().filter(|e| e.event_type == "Failed").collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].payload["Failed"]["reason"], format!("unreconcilable effect {}", patch.effect_id));
    assert_eq!(env.count("EffectUnknown"), 1);
    assert_eq!(env.count("EffectCompleted"), 1, "only the snapshot completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn fence_job_waits_for_a_cooperative_job_and_reports_dead() {
    let fx = Fx::new();
    let exec = Arc::new(fx.scripted("sleep 30"));
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let running = {
        let (exec, req) = (exec.clone(), req.clone());
        tokio::spawn(async move { exec.run(&req, &ctx(1)).await })
    };
    wait_for("a running job", || {
        fx.jobs(&req.effect_id).first().and_then(JobDir::read_status).is_some_and(|s| s.state == JobState::Running)
    })
    .await;
    let started = Instant::now();
    assert!(exec.fence_job(&req.effect_id).await);
    assert!(started.elapsed() < Duration::from_secs(2), "a cooperative job dies without being killed");
    let out = running.await.unwrap();
    assert_eq!(failure_reason(&out), "cancelled");
    let status = fx.job(&req.effect_id).read_status().unwrap();
    assert_eq!((status.state, status.reason), (JobState::Killed, Some(KillReason::Cancel)));
    // Nothing alive: fencing again is immediate.
    assert!(exec.fence_job(&req.effect_id).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn fence_job_kills_a_sigstopped_supervisor_and_its_worker_groups() {
    let fx = Fx::new();
    let pids = fx.path("pids");
    let script = format!("echo $$ > {p}; sleep 30 & echo $! >> {p}; wait", p = pids.display());
    let exec = Arc::new(fx.scripted(&script));
    let req = fx.request(EffectKind::RunVerification, b"");
    let running = {
        let (exec, req) = (exec.clone(), req.clone());
        tokio::spawn(async move { exec.run(&req, &ctx(1)).await })
    };
    wait_for("the script's processes", || fs::read_to_string(&pids).is_ok_and(|s| s.lines().count() == 2)).await;
    let job = fx.job(&req.effect_id);
    // The supervisor writes Running only after the spawn returned, which can be after the script started.
    wait_for("Running status", || job.read_status().is_some_and(|s| s.state == JobState::Running)).await;
    let status = job.read_status().unwrap();
    assert_eq!(status.state, JobState::Running);
    let supervisor = status.supervisor_pid.unwrap() as i32;
    let worker = status.worker_pgid.unwrap();
    let script_pids: Vec<i32> = fs::read_to_string(&pids).unwrap().split_whitespace().map(|p| p.parse().unwrap()).collect();
    assert_eq!(job.groups(), vec![script_pids[0]], "the script's group is recorded");
    let _stopped = Stopped::stop(supervisor);

    let started = Instant::now();
    assert!(exec.fence_job(&req.effect_id).await, "the job is dead after the fence");
    assert!(started.elapsed() >= Duration::from_secs(2), "the stopped supervisor got its 2 s first");
    assert!(job.is_dead());
    let mut all = vec![supervisor, worker];
    all.extend(&script_pids);
    wait_for("every process of the job to die", || all.iter().all(|p| gone(*p))).await;
    // Killed mid-flight: no receipt, and a retryable kind is a failure, not a guess.
    assert_eq!(job.read_receipt(), None);
    let out = running.await.unwrap();
    assert_eq!(failure_reason(&out), "supervisor died without a receipt");
    assert!(!out.unresolved);
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_job_returns_the_receipt_of_a_job_that_finishes_while_waiting() {
    let fx = Fx::new();
    let exec = Arc::new(fx.scripted("sleep 0.5; echo done"));
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    assert_eq!(exec.wait_for_job(&req.effect_id, Duration::from_secs(1)).await, JobWait::Dead, "no job at all");
    let running = {
        let (exec, req) = (exec.clone(), req.clone());
        tokio::spawn(async move { exec.run(&req, &ctx(1)).await })
    };
    wait_for("the job directory", || !fx.jobs(&req.effect_id).is_empty()).await;
    assert!(!fx.job(&req.effect_id).is_dead(), "still running when the wait starts");
    let JobWait::Receipt(found) = exec.wait_for_job(&req.effect_id, PATIENCE).await else { panic!("no receipt") };
    assert_eq!(found.output, b"done\n");
    assert_eq!(running.await.unwrap(), *found);
}

#[tokio::test]
async fn launch_failure_is_a_failure_outcome() {
    let fx = Fx::new();
    let cmd = SupervisorCmd { program: fx.path("no-such-supervisor"), prefix_args: Vec::new() };
    let exec = SupervisedExecutor::new(fx.path("jobs"), cmd, WorkerConfig::Host(host_config(fx.dir.path())), ExecCounts::default())
        .unwrap();
    let (req, ctx) = (fx.request(EffectKind::ReadSnapshot, b""), ctx(1));
    let started = Instant::now();
    let out = exec.run(&req, &ctx).await;
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert!(failure_reason(&out).starts_with("supervisor launch failed: "), "{}", failure_reason(&out));
    assert_for(&out, &req, &ctx);
    assert!(fx.jobs(&req.effect_id).is_empty(), "the job directory is removed again");
    assert_eq!(exec.retained_outcome(&req.effect_id), None);
}

#[tokio::test]
async fn counts_record_each_launch() {
    let fx = Fx::new();
    let exec = fx.scripted("true");
    let snapshot = fx.request(EffectKind::ReadSnapshot, b"");
    // Neither a refused launch nor a failed spawn launches anything.
    let expired = EffectRequest { deadline_ts: 1, ..snapshot.clone() };
    assert_eq!(failure_reason(&exec.run(&expired, &ctx(1)).await), "deadline exceeded");
    let cmd = SupervisorCmd { program: fx.path("no-such-supervisor"), prefix_args: Vec::new() };
    let broken = SupervisedExecutor::new(fx.path("jobs"), cmd, WorkerConfig::Scripted(ScriptedConfig { script: "true".into() }), fx.counts.clone()).unwrap();
    assert!(failure_reason(&broken.run(&snapshot, &ctx(1)).await).starts_with("supervisor launch failed"));
    assert_eq!(fx.counts.get("read_snapshot"), 0);
    exec.run(&snapshot, &ctx(1)).await;
    exec.run(&snapshot, &ctx(2)).await;
    // Another executor sharing the counts (a restarted controller) adds to them.
    fx.scripted("true").run(&fx.request(EffectKind::RunVerification, b""), &ctx(1)).await;
    let counts = exec.counts();
    assert_eq!(counts.get("read_snapshot"), 2);
    assert_eq!(counts.get("run_verification"), 1);
    assert_eq!(counts.get("apply_patch"), 0);
    assert_eq!(fx.counts.get("read_snapshot"), 2);
}

#[tokio::test]
async fn a_job_with_only_request_json_is_waited_for_not_redispatched() {
    let fx = Fx::new();
    let exec = fx.scripted("true");
    let req = fx.request(EffectKind::ReadSnapshot, b"");
    let job_request = |lease_expiry_ms: i64| JobRequest {
        effect_id: req.effect_id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        payload: Vec::new(),
        contract: req.contract.clone(),
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        lease_expiry_ms,
        task_deadline_ms: 0,
        worker: WorkerConfig::Scripted(ScriptedConfig { script: "true".into() }),
    };
    // A launcher that died between creating the job and the supervisor's first status: the
    // lock is held (here by the test), and there is nothing else.
    let (job, lock) = JobDir::create(&fx.path("jobs"), &job_request(now_ms() + 3_600_000)).unwrap();
    assert_eq!(job.read_status(), None);
    assert_eq!(exec.retained_outcome(&req.effect_id), None);
    let started = Instant::now();
    assert_eq!(exec.wait_for_job(&req.effect_id, Duration::from_millis(300)).await, JobWait::StillAlive);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(2), "waited {waited:?}");
    // No pid is known, so the fence can kill nothing: the job stays alive and is not
    // reported dead.
    assert!(!exec.fence_job(&req.effect_id).await);
    assert!(job.cancel_requested());
    assert!(!job.is_dead());
    drop(lock);
    assert_eq!(exec.wait_for_job(&req.effect_id, Duration::from_secs(1)).await, JobWait::Dead);

    // The bound is also the job's own lease plus 5 s: one long past it is not waited for.
    let (_stale, _lock) = JobDir::create(&fx.path("jobs"), &job_request(now_ms() - 60_000)).unwrap();
    let started = Instant::now();
    assert_eq!(exec.wait_for_job(&req.effect_id, PATIENCE).await, JobWait::StillAlive);
    assert!(started.elapsed() < Duration::from_secs(1), "waited {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn fence_never_signals_a_foreign_session_leader_named_in_status_json() {
    let fx = Fx::new();
    // A process that leads its own session, like a supervisor, but is not one.
    let helper = Helper(std::process::Command::new("setsid").args(["sleep", "30"]).spawn().unwrap());
    let pid = helper.0.id();
    wait_for("the helper to lead its session", || {
        rustix::process::getsid(Pid::from_raw(pid as i32)).is_ok_and(|s| s.as_raw_nonzero().get() == pid as i32)
    })
    .await;
    let (job, _lock, effect) = forged_job(&fx, EffectKind::ReadSnapshot, pid, pid as i32);
    let exec = fx.scripted("true");
    assert!(!exec.fence_job(&effect).await, "the job (its lock held by the test) is still alive");
    assert!(!job.is_dead());
    assert!(!gone(pid as i32), "the foreign session leader was signalled");
}

#[tokio::test(flavor = "multi_thread")]
async fn fence_never_signals_another_jobs_supervisor_named_in_status_json() {
    let fx = Fx::new();
    let exec = Arc::new(fx.scripted("sleep 30"));
    let req = fx.request(EffectKind::RunVerification, b"");
    let running = {
        let (exec, req) = (exec.clone(), req.clone());
        tokio::spawn(async move { exec.run(&req, &ctx(1)).await })
    };
    wait_for("a running job", || {
        fx.jobs(&req.effect_id).first().and_then(JobDir::read_status).is_some_and(|s| s.state == JobState::Running)
    })
    .await;
    let real = fx.job(&req.effect_id);
    let status = real.read_status().unwrap();
    let (supervisor, worker) = (status.supervisor_pid.unwrap(), status.worker_pgid.unwrap());
    // Stopped, so it cannot react to anything; only a signal from the fence could kill it.
    let stopped = Stopped::stop(supervisor as i32);
    let (forged, _lock, effect) = forged_job(&fx, EffectKind::ReadSnapshot, supervisor, worker);
    assert!(!exec.fence_job(&effect).await);
    assert!(!forged.is_dead());
    assert!(!gone(supervisor as i32), "another job's supervisor was signalled");
    assert!(!gone(worker), "another job's worker was signalled");
    assert!(!real.is_dead());
    // The real job is still its own: fencing it does kill it.
    assert!(exec.fence_job(&req.effect_id).await);
    drop(stopped);
    assert_eq!(failure_reason(&running.await.unwrap()), "supervisor died without a receipt");
}

#[tokio::test(flavor = "multi_thread")]
async fn fence_kills_a_recorded_group_whose_leader_is_gone() {
    let fx = Fx::new();
    let (req, attempt) = (fx.request(EffectKind::RunVerification, b""), ctx(1));
    let groups = fx.path("jobs").join(format!("{}-{}", req.effect_id, attempt.attempt_id)).join("groups");
    let (member, marker) = (fx.path("member"), fx.path("marker"));
    // The check starts a group whose leader exits (and is reaped) while a member lives on
    // and would write `marker` after 4 s; the group is recorded like any check group.
    let python = format!(
        r#"import os, time
leader = os.fork()
if leader == 0:
    os.setpgid(0, 0)
    if os.fork() == 0:
        open("{member}", "w").write(str(os.getpid()))
        time.sleep(4)
        open("{marker}", "w").close()
    os._exit(0)
with open("{groups}", "a") as f:
    f.write(f"{{leader}}\n")
os.waitpid(leader, 0)
time.sleep(60)
"#,
        member = member.display(),
        marker = marker.display(),
        groups = groups.display(),
    );
    let exec = Arc::new(fx.scripted(&format!("python3 -c '{python}'")));
    let started = Instant::now();
    let running = {
        let (exec, req, attempt) = (exec.clone(), req.clone(), attempt.clone());
        tokio::spawn(async move { exec.run(&req, &attempt).await })
    };
    wait_for("the group member and its recorded group", || {
        fs::read_to_string(&member).is_ok_and(|s| !s.is_empty()) && fs::read_to_string(&groups).is_ok_and(|s| s.lines().count() == 2)
    })
    .await;
    let member_pid: i32 = fs::read_to_string(&member).unwrap().parse().unwrap();
    let job = fx.job(&req.effect_id);
    let leader = *job.groups().last().unwrap();
    assert_ne!(leader, member_pid);
    wait_for("the leader to be reaped", || fs::metadata(format!("/proc/{leader}")).is_err()).await;
    let status = job.read_status().unwrap();
    let _stopped = Stopped::stop(status.supervisor_pid.unwrap() as i32);

    assert!(exec.fence_job(&req.effect_id).await);
    wait_for("the leaderless group's member to die", || gone(member_pid)).await;
    assert_eq!(failure_reason(&running.await.unwrap()), "supervisor died without a receipt");
    let until = Duration::from_millis(4_500);
    if let Some(left) = until.checked_sub(started.elapsed()) {
        tokio::time::sleep(left).await;
    }
    assert!(!marker.exists(), "the member outlived the fence and wrote its marker");
}
