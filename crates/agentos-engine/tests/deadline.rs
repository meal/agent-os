//! The task deadline: checked before each agent turn, intent and dispatch; past it live jobs
//! are waited for or fenced, their receipts published, patches reconciled, nothing is
//! dispatched, and the task fails with `deadline exceeded` (a pending cancel wins).

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_core::effect::{EffectId, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{AgentAction, FakeAgent, Observation};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation};
use agentos_engine::recover::{recover, Decision};
use agentos_engine::runner::{run_task, run_task_with, EngineError};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use common::{
    contract_full, copy_dir, fix_patch, fixtures, supervised, worker_config, workspace_dir, FnAgent, ALL_CAPS, EXIT_BEFORE_RECEIPT_ENV,
    TEST_WORKERS_ENV,
};
use tempfile::TempDir;

fn real_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Patches run under a supervisor that exits after the worker's outcome and before the
/// receipt; everything else runs normally.
struct PatchesWithoutReceipt {
    plain: SupervisedExecutor,
    patches: SupervisedExecutor,
}

impl Executor for PatchesWithoutReceipt {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind.tag() {
            "apply_patch" => self.patches.run(req, ctx).await,
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

    async fn await_job(&self, effect: &EffectId) -> JobWait {
        self.plain.await_job(effect).await
    }

    async fn fence_job(&self, effect: &EffectId) -> bool {
        self.plain.fence_job(effect).await
    }
}

/// A controller over an approved task whose clock the test moves.
struct World {
    dir: TempDir,
    db: Db,
    blobs: BlobStore,
    task: TaskId,
    skew: Arc<AtomicI64>,
    counts: ExecCounts,
}

impl World {
    fn new(deadline_seconds: u32) -> World {
        World::with_profile(deadline_seconds, None)
    }

    /// `check_prefix`: python prepended to the profile's check script.
    fn with_profile(deadline_seconds: u32, check_prefix: Option<&str>) -> World {
        let dir = common::scratch_root();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        if let Some(prefix) = check_prefix {
            let script = dir.path().join("profile/check_parser.py");
            let body = fs::read_to_string(&script).unwrap();
            fs::write(&script, format!("{prefix}\n{body}")).unwrap();
        }
        // The real clock, plus a skew the test can set to jump past the deadline.
        let skew = Arc::new(AtomicI64::new(0));
        let s = skew.clone();
        let db = Db::open(&dir.path().join("agentos.db")).unwrap().with_clock(Box::new(move || real_now() + s.load(Ordering::SeqCst)));
        let (contract, digest) = contract_full(10, ALL_CAPS, deadline_seconds);
        let task = db.create_task(&contract, &digest).unwrap();
        db.approve_task(&task).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        World { dir, db, blobs, task, skew, counts: ExecCounts::default() }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn exec(&self, hook: Option<CrashHook>, env: &[(&str, &str)]) -> SupervisedExecutor {
        supervised(&self.path("jobs"), worker_config(self.dir.path()), &self.counts, hook, env)
    }

    /// Moves the clock past the task's deadline.
    fn expire(&self) {
        self.skew.store(self.db.deadline_ts(&self.task).unwrap() + 1 - real_now(), Ordering::SeqCst);
    }

    fn jobs(&self) -> usize {
        fs::read_dir(self.path("jobs")).map(|d| d.count()).unwrap_or(0)
    }

    fn events(&self) -> Vec<(String, serde_json::Value)> {
        self.db.events(&self.task).unwrap().into_iter().map(|e| (e.event_type, e.payload)).collect()
    }

    fn count(&self, ty: &str) -> usize {
        self.events().iter().filter(|(t, _)| t == ty).count()
    }

    fn failed_reason(&self) -> Option<String> {
        self.events().iter().find(|(t, _)| t == "Failed").map(|(_, p)| p["Failed"]["reason"].as_str().unwrap().to_string())
    }

    fn disk(&self) -> Digest {
        workspace_digest(&workspace_dir(self.dir.path(), &self.task)).unwrap()
    }

    /// Runs the fixture solution until the hook crashes the controller.
    async fn crash_at(&self, hook: CrashHook) {
        let exec = self.exec(Some(hook.clone()), &[]);
        self.crash_with(hook, &exec).await;
    }

    /// As [`World::crash_at`], with patches that leave no receipt.
    async fn crash_at_without_patch_receipt(&self, hook: CrashHook) {
        let exec = PatchesWithoutReceipt {
            plain: self.exec(Some(hook.clone()), &[]),
            patches: self.exec(Some(hook.clone()), &[(TEST_WORKERS_ENV, "1"), (EXIT_BEFORE_RECEIPT_ENV, "1")]),
        };
        self.crash_with(hook, &exec).await;
    }

    async fn crash_with(&self, hook: CrashHook, exec: &impl Executor) {
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let err = run_task_with(&self.db, &self.blobs, exec, &mut agent, &self.task, &RunOptions::crash_with(hook)).await;
        assert!(matches!(err, Err(EngineError::Crashed(_))), "{err:?}: {:?}", self.events().iter().map(|(t, p)| format!("{t} {}", &p.to_string()[..p.to_string().len().min(160)])).collect::<Vec<_>>());
    }
}

/// No process whose command line mentions the world's jobs directory is left.
fn assert_no_job_processes(w: &World) {
    let needle = w.path("jobs").to_string_lossy().into_owned();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
        assert!(!String::from_utf8_lossy(&cmdline).contains(&needle), "a job process survives: {}", entry.path().display());
    }
}

#[tokio::test]
async fn deadline_between_turns_fails_the_task_without_new_effects() {
    let w = World::new(600);
    let exec = w.exec(None, &[]);
    let skew = w.skew.clone();
    let deadline = w.db.deadline_ts(&w.task).unwrap();
    let mut agent = FnAgent(move |obs: &Observation| match obs {
        // The agent "thinks" past the deadline.
        Observation::Start { .. } => {
            skew.store(deadline + 1 - real_now(), Ordering::SeqCst);
            AgentAction::ApplyPatch(fix_patch())
        }
        _ => AgentAction::Finish,
    });
    let state = run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task).await.unwrap();
    assert_eq!(state, TaskState::Failed);
    assert_eq!(w.failed_reason().as_deref(), Some("deadline exceeded"));
    assert_eq!(w.count("EffectIntended"), 1, "only the snapshot was ever intended");
    assert_eq!(w.jobs(), 1);
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn deadline_during_a_verification_kills_the_job_and_fails_the_task() {
    // The deadline (whole seconds) must fall inside the verification: after the snapshot's
    // and the patch's jobs. Real VMs boot in about 0.7 s each, so the real worker gets more
    // headroom before the 30 s check than the 4 s the fast workers need.
    let deadline = if common::real_mode() { 8 } else { 4 };
    let w = World::with_profile(deadline, Some("import time\ntime.sleep(30)"));
    // The supervisor enforces the deadline on its own, by the real clock.
    let exec = w.exec(None, &[]);
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let started = std::time::Instant::now();
    let state = run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task).await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "the 30 s check was killed at the deadline");
    assert_eq!(state, TaskState::Failed);
    assert_eq!(w.failed_reason().as_deref(), Some("deadline exceeded"));
    let failures: Vec<String> =
        w.events().into_iter().filter(|(t, _)| t == "EffectFailed").map(|(_, p)| p.to_string()).collect();
    assert!(failures.iter().any(|p| p.contains("deadline exceeded")), "{failures:?}");
    let usage = w.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.reserved_tool_actions, usage.reserved_model_requests), (0, 0), "{usage:?}");
    assert!(w.db.outstanding_effects(&w.task).unwrap().is_empty());
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn deadline_during_apply_patch_reconciles_before_failing() {
    let w = World::new(600);
    // The patch applies, then its supervisor dies before writing a receipt; the controller dies too.
    let hook = CrashHook::at(CrashPoint::DuringExecute, "apply_patch");
    w.crash_at_without_patch_receipt(hook).await;
    w.expire();

    let exec = w.exec(None, &[]);
    let report = recover(&w.db, &w.blobs, &exec, &w.task).await.unwrap();
    assert_eq!(report.state, Some(TaskState::Failed));
    assert_eq!(w.failed_reason().as_deref(), Some("deadline exceeded"));
    let decisions: Vec<Decision> = report.decisions.iter().map(|d| d.decision).collect();
    assert!(decisions.contains(&Decision::PublishReconciled), "the applied patch was published, not failed: {decisions:?}");
    let types: Vec<String> = w.events().into_iter().map(|(t, _)| t).collect();
    let completed = types.iter().rposition(|t| t == "EffectCompleted").unwrap();
    let failed = types.iter().position(|t| t == "Failed").unwrap();
    assert!(completed < failed, "the patch was published before the task failed: {types:?}");
    assert_eq!(w.count("EffectFailed"), 0);
    assert_eq!(w.db.task(&w.task).unwrap().workspace_digest, w.disk(), "the journal matches the disk");
    assert_ne!(w.disk(), workspace_digest(&w.path("snapshot")).unwrap(), "the patch really applied");
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn deadline_expired_before_recovery_runs_still_reconciles_then_fails_and_dispatches_nothing() {
    let w = World::new(600);
    w.crash_at(CrashHook::at(CrashPoint::AfterIntent, "apply_patch")).await;
    let (jobs, dispatched) = (w.jobs(), w.count("EffectDispatched"));
    w.expire();

    let exec = w.exec(None, &[]);
    let report = recover(&w.db, &w.blobs, &exec, &w.task).await.unwrap();
    assert_eq!(report.state, Some(TaskState::Failed));
    assert_eq!(w.failed_reason().as_deref(), Some("deadline exceeded"));
    assert_eq!(w.jobs(), jobs, "no job directory after the deadline");
    assert_eq!(w.count("EffectDispatched"), dispatched);
    assert_eq!(report.abandoned.len(), 1, "the intended patch was abandoned: {report:?}");
    let patch = w.db.outstanding_effects(&w.task).unwrap();
    assert!(patch.iter().all(|e| e.state != EffectState::Dispatched), "{patch:?}");
    let usage = w.db.usage_summary(&w.task).unwrap();
    assert_eq!(usage.reserved_tool_actions, 0, "{usage:?}");
    // Recovering again changes nothing.
    let events = w.events().len();
    recover(&w.db, &w.blobs, &exec, &w.task).await.unwrap();
    assert_eq!(w.events().len(), events);
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn cancel_wins_over_deadline() {
    let w = World::new(600);
    w.crash_at(CrashHook::at(CrashPoint::AfterIntent, "apply_patch")).await;
    w.db.append(&w.task, &TaskEvent::CancelRequested).unwrap();
    w.expire();

    let exec = w.exec(None, &[]);
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let state = run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task).await.unwrap();
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(w.failed_reason(), None);
}

#[tokio::test]
async fn unapproved_task_has_no_deadline_and_a_late_approval_starts_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(1_000));
    let c = clock.clone();
    let db = Db::open(&dir.path().join("agentos.db")).unwrap().with_clock(Box::new(move || c.load(Ordering::SeqCst)));
    let (contract, digest) = contract_full(10, ALL_CAPS, 600);
    let task = db.create_task(&contract, &digest).unwrap();
    assert_eq!(db.deadline_ts(&task).unwrap(), 0);
    clock.store(1_000_000, Ordering::SeqCst);
    assert!(!db.deadline_passed(&task).unwrap(), "no deadline before approval, however late it is");
    db.approve_task(&task).unwrap();
    assert_eq!(db.deadline_ts(&task).unwrap(), 1_000_600);
    assert!(!db.deadline_passed(&task).unwrap());
    clock.store(1_000_600, Ordering::SeqCst);
    assert!(db.deadline_passed(&task).unwrap());
}
