//! Revocation: a revoked capability stops the running jobs that need it, is denied at the
//! next authorization (including a dispatch authorized by an earlier intent and recovery's
//! redispatch), and ends the task through recovery's closing mode; results already in the
//! journal stay.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentos_core::contract::Capability;
use agentos_core::effect::EffectId;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::{AgentAction, FakeAgent};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{
    AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation,
};
use agentos_engine::recover::recover;
use agentos_engine::runner::{EngineError, run_task, run_task_with};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use common::{
    ALL_CAPS, EXIT_BEFORE_RECEIPT_ENV, TEST_WORKERS_ENV, contract_full, copy_dir, fix_patch,
    fixtures, supervised, worker_config,
};
use tempfile::TempDir;

/// Effects of `kind` run under a supervisor that exits after the worker's outcome and before
/// the receipt; everything else runs normally.
struct WithoutReceipt {
    kind: &'static str,
    plain: SupervisedExecutor,
    special: SupervisedExecutor,
}

impl Executor for WithoutReceipt {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind.tag() == self.kind {
            true => self.special.run(req, ctx).await,
            false => self.plain.run(req, ctx).await,
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

/// A controller over an approved task.
struct World {
    dir: TempDir,
    db: Db,
    blobs: BlobStore,
    task: TaskId,
    counts: ExecCounts,
}

impl World {
    fn new(deadline_seconds: u32) -> World {
        World::with_profile(deadline_seconds, None)
    }

    /// `check_prefix`: python prepended to the profile's check script.
    fn with_profile(deadline_seconds: u32, check_prefix: Option<&str>) -> World {
        let dir = common::scratch_root();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        if let Some(prefix) = check_prefix {
            let script = dir.path().join("profile/check_parser.py");
            let body = fs::read_to_string(&script).unwrap();
            fs::write(&script, format!("{prefix}\n{body}")).unwrap();
        }
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let (contract, digest) = contract_full(10, ALL_CAPS, deadline_seconds);
        let task = db.create_task(&contract, &digest).unwrap();
        db.approve_task(&task).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        World {
            dir,
            db,
            blobs,
            task,
            counts: ExecCounts::default(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn exec(&self, hook: Option<CrashHook>, env: &[(&str, &str)]) -> SupervisedExecutor {
        supervised(
            &self.path("jobs"),
            worker_config(self.dir.path()),
            &self.counts,
            hook,
            env,
        )
    }

    fn jobs(&self) -> usize {
        fs::read_dir(self.path("jobs"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    fn events(&self) -> Vec<(String, serde_json::Value)> {
        self.db
            .events(&self.task)
            .unwrap()
            .into_iter()
            .map(|e| (e.event_type, e.payload))
            .collect()
    }

    fn count(&self, ty: &str) -> usize {
        self.events().iter().filter(|(t, _)| t == ty).count()
    }

    fn failed_reason(&self) -> Option<String> {
        self.events()
            .iter()
            .find(|(t, _)| t == "Failed")
            .map(|(_, p)| p["Failed"]["reason"].as_str().unwrap().to_string())
    }

    /// Runs the fixture solution until the hook crashes the controller.
    async fn crash_at(&self, hook: CrashHook) {
        let exec = self.exec(Some(hook.clone()), &[]);
        self.crash_with(hook, &exec).await;
    }

    /// As [`World::crash_at`], with `kind` effects that leave no receipt.
    async fn crash_at_without_receipt(&self, hook: CrashHook, kind: &'static str) {
        let exec = WithoutReceipt {
            kind,
            plain: self.exec(Some(hook.clone()), &[]),
            special: self.exec(
                Some(hook.clone()),
                &[(TEST_WORKERS_ENV, "1"), (EXIT_BEFORE_RECEIPT_ENV, "1")],
            ),
        };
        self.crash_with(hook, &exec).await;
    }

    async fn crash_with(&self, hook: CrashHook, exec: &impl Executor) {
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let err = run_task_with(
            &self.db,
            &self.blobs,
            exec,
            &mut agent,
            &self.task,
            &RunOptions::crash_with(hook),
        )
        .await;
        assert!(
            matches!(err, Err(EngineError::Crashed(_))),
            "{err:?}: {:?}",
            self.events()
                .iter()
                .map(|(t, p)| format!("{t} {}", &p.to_string()[..p.to_string().len().min(160)]))
                .collect::<Vec<_>>()
        );
    }
}

/// No process whose command line mentions the world's jobs directory is left.
fn assert_no_job_processes(w: &World) {
    let needle = w.path("jobs").to_string_lossy().into_owned();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        assert!(
            !String::from_utf8_lossy(&cmdline).contains(&needle),
            "a job process survives: {}",
            entry.path().display()
        );
    }
}

const SLOW_CHECK: &str = "import time\ntime.sleep(30)";

fn revoke(w: &World, db: &Db, only: Capability) -> Vec<Capability> {
    let _ = w;
    db.revoke(&w.task, Some(only)).unwrap()
}

/// Waits until the verification job has a status (its supervisor is up).
async fn wait_for_verification_job(w: &World) {
    let started = Instant::now();
    loop {
        let up = fs::read_dir(w.path("jobs"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.path().join("status.json").is_file()
                    && fs::read_to_string(e.path().join("request.json"))
                        .is_ok_and(|r| r.contains("RunVerification"))
            });
        if up {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the verification job never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What `agentos revoke` does after `Db::revoke`: stop the live jobs whose kind needs a
/// revoked capability.
fn cancel_for(w: &World, exec: &SupervisedExecutor, revoked: &[Capability]) -> usize {
    let effects: Vec<EffectId> =
        w.db.outstanding_effects(&w.task)
            .unwrap()
            .into_iter()
            .filter(|e| revoked.contains(&e.kind.capability()))
            .map(|e| e.effect_id)
            .collect();
    exec.cancel_jobs(&effects)
}

fn denials(w: &World, reason: &str) -> usize {
    w.events()
        .iter()
        .filter(|(t, p)| t == "CapabilityDenied" && p["reason"] == reason)
        .count()
}

#[tokio::test]
async fn an_intent_authorized_before_a_revoke_is_denied_at_dispatch() {
    let w = World::new(600);
    w.crash_at(CrashHook::at(CrashPoint::AfterIntent, "apply_patch"))
        .await;
    let jobs = w.jobs();
    assert_eq!(
        revoke(&w, &w.db, Capability::WorkspaceApplyPatch),
        vec![Capability::WorkspaceApplyPatch]
    );

    let exec = w.exec(None, &[]);
    let report = recover(&w.db, &w.blobs, &exec, &w.task).await.unwrap();
    assert_eq!(report.state, Some(TaskState::Failed));
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("capability revoked: workspace.apply_patch")
    );
    assert_eq!(
        denials(&w, "revoked"),
        1,
        "the dispatch denial is journaled once"
    );
    assert_eq!(w.jobs(), jobs, "no job directory for the refused dispatch");
    assert_eq!(
        w.count("EffectDispatched"),
        1,
        "only the snapshot was dispatched"
    );
    assert_eq!(
        w.count("EffectAbandoned"),
        1,
        "the intended patch was abandoned"
    );
    assert_eq!(
        w.db.usage_summary(&w.task).unwrap().reserved_tool_actions,
        0
    );
    assert!(w.db.outstanding_effects(&w.task).unwrap().is_empty());
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn recovery_redispatch_of_a_revoked_capability_closes_the_task_instead_of_launching() {
    let w = World::new(600);
    // The verification job dies without a receipt: recovery would run it again.
    w.crash_at_without_receipt(
        CrashHook::at(CrashPoint::DuringExecute, "run_verification"),
        "run_verification",
    )
    .await;
    let verification = w.db.outstanding_effects(&w.task).unwrap().remove(0);
    assert_eq!(verification.kind.capability(), Capability::VerificationRun);
    let launches = w.counts.get("run_verification");
    revoke(&w, &w.db, Capability::VerificationRun);

    let exec = w.exec(None, &[]);
    let report = recover(&w.db, &w.blobs, &exec, &w.task).await.unwrap();
    assert_eq!(report.state, Some(TaskState::Failed));
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("capability revoked: verification.run")
    );
    assert_eq!(
        w.counts.get("run_verification"),
        launches,
        "no second launch"
    );
    assert_eq!(denials(&w, "revoked"), 1);
    let usage = w.db.usage_summary(&w.task).unwrap();
    assert_eq!(usage.reserved_tool_actions, 0, "{usage:?}");
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn revoked_snapshot_read_stops_a_resumed_task_with_failed_not_stuck() {
    let w = World::new(600);
    w.crash_at(CrashHook::at(CrashPoint::AfterIntent, "read_snapshot"))
        .await;
    revoke(&w, &w.db, Capability::SnapshotRead);

    let exec = w.exec(None, &[]);
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let state = run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task)
        .await
        .unwrap();
    assert_eq!(state, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("capability revoked: snapshot.read")
    );
    assert!(w.db.outstanding_effects(&w.task).unwrap().is_empty());
    assert_eq!(w.jobs(), 0);
}

#[tokio::test]
async fn revoke_verification_run_kills_the_running_check_and_the_next_request_is_denied_revoked_while_results_stay_in_the_journal()
 {
    let w = World::with_profile(600, Some(SLOW_CHECK));
    let exec = w.exec(None, &[]);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
        AgentAction::Verify,
    ]);
    let other = Db::open(&w.path("agentos.db")).unwrap();
    let started = Instant::now();
    let steer = async {
        wait_for_verification_job(&w).await;
        let revoked = revoke(&w, &other, Capability::VerificationRun);
        assert_eq!(
            cancel_for(&w, &exec, &revoked),
            1,
            "the running check was asked to stop"
        );
    };
    let (state, ()) = tokio::join!(run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task), steer);
    assert_eq!(state.unwrap(), TaskState::Failed);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the 30 s check was killed, took {:?}",
        started.elapsed()
    );
    let failed: Vec<String> = w
        .events()
        .into_iter()
        .filter(|(t, _)| t == "EffectFailed")
        .map(|(_, p)| p.to_string())
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "the killed check is a recorded failure: {failed:?}"
    );
    assert!(failed[0].contains("cancelled"), "{failed:?}");
    assert_eq!(
        w.count("EffectCompleted"),
        2,
        "the snapshot and patch results stay"
    );
    let denied: Vec<_> = w
        .events()
        .into_iter()
        .filter(|(t, p)| t == "Denied" && p["action"] == "Verify")
        .collect();
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0].1["denial"], "revoked");
    assert_eq!(
        w.count("EffectIntended"),
        3,
        "no verification was intended after the revoke"
    );
    assert_no_job_processes(&w);
}

#[tokio::test]
async fn revoke_one_capability_keeps_other_running_effects() {
    // The check sleeps 2 s, then passes.
    let w = World::with_profile(600, Some("import time\ntime.sleep(2)"));
    let exec = w.exec(None, &[]);
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    let other = Db::open(&w.path("agentos.db")).unwrap();
    let steer = async {
        wait_for_verification_job(&w).await;
        // snapshot.read is no business of the running verification.
        let revoked = revoke(&w, &other, Capability::SnapshotRead);
        assert_eq!(cancel_for(&w, &exec, &revoked), 0);
    };
    let (state, ()) = tokio::join!(run_task(&w.db, &w.blobs, &exec, &mut agent, &w.task), steer);
    assert_eq!(state.unwrap(), TaskState::Succeeded);
    assert_eq!(w.count("EffectFailed"), 0);
}
