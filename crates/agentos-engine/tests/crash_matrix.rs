//! Crash injection and recovery. Every test simulates a controller kill by returning
//! `EngineError::Crashed` at a boundary and then dropping every in-memory handle (database,
//! blob store, executor, agent); the restarted controller reopens the same on-disk paths
//! with fresh objects.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use agentos_core::budget::Reservation;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectRecord, EffectState, Outcome, Receipt, ReceiptVerdict};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{AgentAction, FakeAgent};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::durable::{DurableExecutor, ExecCounts};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::recover::{recover, recover_with, Decision, RecoveryReport};
use agentos_engine::runner::{run_task, run_task_with, EngineError};
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use agentos_store::effects::UsageSummary;
use common::{comment_patch, contract, copy_dir, fix_patch, fixtures};
use tempfile::TempDir;

const KINDS: [&str; 3] = ["read_snapshot", "apply_patch", "run_verification"];

/// Everything on disk that survives a controller kill.
struct World {
    dir: TempDir,
    task: TaskId,
    counts: ExecCounts,
}

/// The in-memory controller: dropped wholesale to simulate a kill.
struct Ctl {
    db: Db,
    blobs: BlobStore,
    exec: DurableExecutor<FixtureExecutor>,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let (contract, digest) = contract(10);
        let task = db.create_task(&contract, &digest).unwrap();
        World { dir, task, counts: ExecCounts::default() }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// A freshly restarted controller over this world's files.
    fn open(&self, hook: Option<CrashHook>) -> Ctl {
        let fixture = FixtureExecutor::new(self.path("snapshot"), self.path("profile"), self.path("work"));
        Ctl {
            db: Db::open(&self.path("agentos.db")).unwrap(),
            blobs: BlobStore::open(self.path("blobs")).unwrap(),
            exec: DurableExecutor::new(fixture, self.path("receipts"), self.counts.clone()).unwrap().with_crash(hook),
        }
    }

    fn ws(&self) -> PathBuf {
        self.path("work").join(self.task.as_str()).join("ws")
    }

    fn base(&self) -> Digest {
        workspace_digest(&self.path("snapshot")).unwrap()
    }

    /// Digests of every object file in the blob store.
    fn blobs_on_disk(&self) -> BTreeSet<Digest> {
        let mut out = BTreeSet::new();
        for shard in std::fs::read_dir(self.path("blobs/objects")).unwrap() {
            let shard = shard.unwrap();
            for obj in std::fs::read_dir(shard.path()).unwrap() {
                let hex = format!("{}{}", shard.file_name().to_str().unwrap(), obj.unwrap().file_name().to_str().unwrap());
                out.insert(Digest::from_hex(&hex).unwrap());
            }
        }
        out
    }

    fn counts(&self) -> HashMap<&'static str, usize> {
        KINDS.iter().map(|k| (*k, self.counts.get(k))).collect()
    }

    /// Runs the scripted fixture solution with a fresh agent.
    async fn run(&self, ctl: &Ctl, opts: &RunOptions) -> Result<TaskState, EngineError> {
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        run_task_with(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &self.task, opts).await
    }

    /// Runs until the hook fires, then drops the controller (the kill).
    async fn crash_run(&self, hook: CrashHook) -> CrashPoint {
        let ctl = self.open(Some(hook.clone()));
        let err = self.run(&ctl, &RunOptions::crash_with(hook)).await.unwrap_err();
        match err {
            EngineError::Crashed(p) => p,
            other => panic!("expected an injected crash, got {other:?}"),
        }
    }
}

impl Ctl {
    fn events(&self, w: &World) -> Vec<StoredEvent> {
        self.db.events(&w.task).unwrap()
    }

    fn of_type(&self, w: &World, event_type: &str) -> Vec<serde_json::Value> {
        self.events(w).into_iter().filter(|e| e.event_type == event_type).map(|e| e.payload).collect()
    }

    /// Effects in intent order.
    fn effects(&self, w: &World) -> Vec<EffectRecord> {
        self.of_type(w, "EffectIntended")
            .into_iter()
            .map(|p| self.db.effect(&serde_json::from_value(p["effect_id"].clone()).unwrap()).unwrap())
            .collect()
    }

    fn effect(&self, w: &World, tag: &str) -> EffectRecord {
        let mut found: Vec<_> = self.effects(w).into_iter().filter(|e| e.kind.tag() == tag).collect();
        assert_eq!(found.len(), 1, "exactly one {tag} effect");
        found.remove(0)
    }

    async fn recover(&self, w: &World) -> RecoveryReport {
        recover(&self.db, &self.blobs, &self.exec, &w.task).await.unwrap()
    }
}

/// The normalized end state a run must reach whatever crashed along the way. Ids, leases
/// and the journal differ between runs; none of these may.
#[derive(Debug, PartialEq)]
struct Summary {
    state: TaskState,
    task_workspace: Digest,
    ws_on_disk: Digest,
    verified: Option<Digest>,
    actions_used: u32,
    effects: Vec<(&'static str, EffectState)>,
    usage: UsageSummary,
    snapshot_manifest: Option<Digest>,
    patch_request: Digest,
    patch_result: Option<Digest>,
    evidence: Option<Digest>,
    blobs: BTreeSet<Digest>,
}

fn summarize(w: &World, ctl: &Ctl) -> Summary {
    let task = ctl.db.task(&w.task).unwrap();
    let effects = ctl.effects(w);
    let referenced = ctl.db.referenced_blobs().unwrap().into_iter().collect::<BTreeSet<_>>();
    let on_disk = w.blobs_on_disk();
    assert_eq!(on_disk, referenced, "every stored blob is referenced and every referenced blob is stored");
    let result = |tag: &str| ctl.effect(w, tag).result_digest;
    Summary {
        state: task.state,
        task_workspace: task.workspace_digest,
        ws_on_disk: workspace_digest(&w.ws()).unwrap(),
        verified: task.verified_digest,
        actions_used: task.actions_used,
        effects: effects.iter().map(|e| (e.kind.tag(), e.state)).collect(),
        usage: ctl.db.usage_summary(&w.task).unwrap(),
        snapshot_manifest: result("read_snapshot"),
        patch_request: ctl.effect(w, "apply_patch").request_digest,
        patch_result: result("apply_patch"),
        evidence: result("run_verification"),
        blobs: on_disk,
    }
}

async fn baseline() -> Summary {
    let w = World::new();
    let ctl = w.open(None);
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    for k in KINDS {
        assert_eq!(w.counts.get(k), 1, "{k}");
    }
    let s = summarize(&w, &ctl);
    assert_eq!(s.verified, Some(s.ws_on_disk));
    assert_ne!(s.ws_on_disk, w.base());
    s
}

/// Journal invariants every run must keep, crashed or not.
fn assert_journal_sound(w: &World, ctl: &Ctl) {
    let events = ctl.events(w);
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>(), "gapless sequence");
    let mut completed = HashMap::new();
    for e in events.iter().filter(|e| e.event_type == "EffectCompleted" || e.event_type == "EffectFailed") {
        *completed.entry(e.payload["effect_id"].as_str().unwrap().to_string()).or_insert(0) += 1;
    }
    assert!(completed.values().all(|n| *n == 1), "an effect completed twice: {completed:?}");
}

fn expected_decision(point: CrashPoint, kind: &str) -> Option<Decision> {
    match point {
        CrashPoint::AfterIntent => Some(Decision::Dispatch),
        CrashPoint::AfterDispatch => Some(Decision::Redispatch),
        CrashPoint::DuringExecute if kind == "apply_patch" => Some(Decision::PublishReconciled),
        CrashPoint::DuringExecute => Some(Decision::Redispatch),
        CrashPoint::AfterExecuteBeforePublish | CrashPoint::AfterBlobPut | CrashPoint::AfterRegister => {
            Some(Decision::PublishRetained)
        }
        CrashPoint::AfterComplete | CrashPoint::AfterAgentTurnJournaled => None,
    }
}

/// Real executions of `tag` once a crash at `point` in `kind` was recovered. Only an
/// attempt that ran without leaving a durable receipt may run again, and only when its kind
/// is safe to retry blindly; the patch is reconciled instead.
fn expected_executions(point: CrashPoint, kind: &str, tag: &str) -> usize {
    if point == CrashPoint::DuringExecute && kind == tag && kind != "apply_patch" { 2 } else { 1 }
}

async fn crash_case(point: CrashPoint, kind: &'static str) {
    let baseline = baseline().await;
    let w = World::new();
    assert_eq!(w.crash_run(CrashHook::at(point, kind)).await, point);

    let ctl = w.open(None);
    let outstanding: Vec<EffectId> =
        ctl.db.outstanding_effects(&w.task).unwrap().into_iter().map(|e| e.effect_id).collect();
    let referenced_before = ctl.db.referenced_blobs().unwrap();
    let leftovers: BTreeSet<_> = w.blobs_on_disk().into_iter().filter(|d| !referenced_before.contains(d)).collect();
    if point == CrashPoint::AfterBlobPut {
        assert!(!leftovers.is_empty(), "the crash left an unregistered blob behind");
    }

    let report = ctl.recover(&w).await;

    assert!(report.gc_removed >= leftovers.len(), "{report:?} vs {leftovers:?}");
    for d in &leftovers {
        assert!(!w.blobs_on_disk().contains(d) || ctl.db.referenced_blobs().unwrap().contains(d), "leftover {d} kept");
    }
    for d in &referenced_before {
        assert!(w.blobs_on_disk().contains(d), "referenced blob {d} removed by gc");
    }
    let decided: Vec<_> = report.decisions.iter().map(|d| d.effect_id.clone()).collect();
    assert_eq!(decided, outstanding, "one decision per outstanding effect, in creation order");
    match expected_decision(point, kind) {
        Some(decision) => {
            assert_eq!(report.decisions.len(), 1);
            assert_eq!(report.decisions[0].decision, decision, "{report:?}");
            assert_eq!(report.decisions[0].kind, kind);
            let journaled = ctl.of_type(&w, "RecoveryDecision");
            assert_eq!(journaled.len(), 1);
            assert_eq!(journaled[0]["decision"], serde_json::to_value(decision).unwrap());
            assert_eq!(journaled[0]["effect_id"], serde_json::to_value(&outstanding[0]).unwrap());
        }
        None => assert!(report.decisions.is_empty() && outstanding.is_empty(), "{report:?}"),
    }
    assert!(ctl.db.outstanding_effects(&w.task).unwrap().is_empty());
    assert_eq!(workspace_digest(&w.ws()).unwrap(), ctl.db.task(&w.task).unwrap().workspace_digest, "journal and workspace agree");

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    assert_eq!(summarize(&w, &ctl), baseline, "crash at {point} in {kind}");
    for tag in KINDS {
        assert_eq!(w.counts.get(tag), expected_executions(point, kind, tag), "executions of {tag}");
    }
    let lease = if expected_decision(point, kind) == Some(Decision::Redispatch) { 2 } else { 1 };
    assert_eq!(ctl.effect(&w, kind).lease_generation, lease);
    assert_journal_sound(&w, &ctl);
    assert_eq!(ctl.of_type(&w, "ReceiptIgnored").len(), 0);

    // Recovering a recovered, finished task changes nothing.
    let events = ctl.events(&w);
    let again = ctl.recover(&w).await;
    assert!(again.decisions.is_empty() && again.gc_removed == 0 && again.abandoned.is_empty(), "{again:?}");
    assert_eq!(ctl.events(&w), events);
    assert_eq!(summarize(&w, &ctl), baseline);
}

macro_rules! matrix {
    ($($name:ident: $point:ident, $kind:literal;)*) => {
        $(
            #[tokio::test]
            async fn $name() {
                crash_case(CrashPoint::$point, $kind).await;
            }
        )*
    };
}

matrix! {
    snapshot_after_intent: AfterIntent, "read_snapshot";
    snapshot_after_dispatch: AfterDispatch, "read_snapshot";
    snapshot_during_execute: DuringExecute, "read_snapshot";
    snapshot_after_execute_before_publish: AfterExecuteBeforePublish, "read_snapshot";
    snapshot_after_blob_put: AfterBlobPut, "read_snapshot";
    snapshot_after_register: AfterRegister, "read_snapshot";
    snapshot_after_complete: AfterComplete, "read_snapshot";
    patch_after_agent_turn: AfterAgentTurnJournaled, "apply_patch";
    patch_after_intent: AfterIntent, "apply_patch";
    patch_after_dispatch: AfterDispatch, "apply_patch";
    patch_during_execute: DuringExecute, "apply_patch";
    patch_after_execute_before_publish: AfterExecuteBeforePublish, "apply_patch";
    patch_after_blob_put: AfterBlobPut, "apply_patch";
    patch_after_register: AfterRegister, "apply_patch";
    patch_after_complete: AfterComplete, "apply_patch";
    verify_after_agent_turn: AfterAgentTurnJournaled, "run_verification";
    verify_after_intent: AfterIntent, "run_verification";
    verify_after_dispatch: AfterDispatch, "run_verification";
    verify_during_execute: DuringExecute, "run_verification";
    verify_after_execute_before_publish: AfterExecuteBeforePublish, "run_verification";
    verify_after_blob_put: AfterBlobPut, "run_verification";
    verify_after_register: AfterRegister, "run_verification";
    verify_after_complete: AfterComplete, "run_verification";
}

#[tokio::test]
async fn baseline_is_reproducible_across_worlds() {
    assert_eq!(baseline().await, baseline().await);
}

#[tokio::test]
async fn run_task_recovers_outstanding_effects_by_itself() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterRegister, "apply_patch")).await;
    let ctl = w.open(None);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 1);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

#[tokio::test]
async fn recovering_twice_before_resuming_is_idempotent() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch")).await;
    let ctl = w.open(None);
    let first = ctl.recover(&w).await;
    assert_eq!(first.decisions.len(), 1);
    let (events, effects, usage, blobs) =
        (ctl.events(&w), ctl.effects(&w), ctl.db.usage_summary(&w.task).unwrap(), w.blobs_on_disk());

    let second = recover(&ctl.db, &ctl.blobs, &ctl.exec, &w.task).await.unwrap();

    assert!(second.decisions.is_empty() && second.gc_removed == 0, "{second:?}");
    assert_eq!(ctl.events(&w), events);
    assert_eq!(ctl.effects(&w), effects);
    assert_eq!(ctl.db.usage_summary(&w.task).unwrap(), usage);
    assert_eq!(w.blobs_on_disk(), blobs);
    assert_eq!(w.counts.get("apply_patch"), 1);
}

#[tokio::test]
async fn a_crash_during_recovery_still_converges() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "apply_patch")).await;
    {
        // The first recovery reconciles the patch, then dies before completing it.
        let hook = CrashHook::at(CrashPoint::AfterRegister, "apply_patch");
        let ctl = w.open(Some(hook.clone()));
        let err = recover_with(&ctl.db, &ctl.blobs, &ctl.exec, &w.task, &RunOptions::crash_with(hook)).await;
        assert!(matches!(err, Err(EngineError::Crashed(CrashPoint::AfterRegister))), "{err:?}");
    }
    let ctl = w.open(None);
    let report = ctl.recover(&w).await;
    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::PublishRetained, "the reconciled outcome was retained");

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("apply_patch"), 1, "the patch ran once");
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn repeated_crashes_of_a_retried_verification_converge() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "run_verification")).await;
    {
        let hook = CrashHook::at(CrashPoint::DuringExecute, "run_verification");
        let ctl = w.open(Some(hook.clone()));
        let err = recover_with(&ctl.db, &ctl.blobs, &ctl.exec, &w.task, &RunOptions::crash_with(hook)).await;
        assert!(matches!(err, Err(EngineError::Crashed(CrashPoint::DuringExecute))), "{err:?}");
    }
    let ctl = w.open(None);
    let report = ctl.recover(&w).await;
    assert_eq!(report.decisions[0].decision, Decision::Redispatch);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("run_verification"), 2, "the receipt-less attempt and the final one");
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 3);
    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 2);
    assert_journal_sound(&w, &ctl);
}

fn request(ctl: &Ctl, w: &World, rec: &EffectRecord) -> EffectRequest {
    EffectRequest {
        effect_id: rec.effect_id.clone(),
        task_id: w.task.clone(),
        kind: rec.kind.clone(),
        payload: Vec::new(),
        contract: ctl.db.contract(&w.task).unwrap(),
    }
}

fn retained(ctl: &Ctl, effect: &EffectId) -> ExecOutcome {
    ctl.exec.retained_outcome(effect).expect("the executor retained an outcome")
}

fn attempt(lease: u64) -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: lease, worker: "zombie".into() }
}

#[tokio::test]
async fn stale_and_duplicate_receipts_are_ignored_and_audited() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "run_verification")).await;
    {
        // Recovery re-dispatches under lease 2, then dies before executing.
        let hook = CrashHook::new(|p, ctx| p == CrashPoint::AfterDispatch && ctx.kind == Some("run_verification"));
        let ctl = w.open(Some(hook.clone()));
        let err = recover_with(&ctl.db, &ctl.blobs, &ctl.exec, &w.task, &RunOptions::crash_with(hook)).await;
        assert!(matches!(err, Err(EngineError::Crashed(CrashPoint::AfterDispatch))), "{err:?}");
    }
    let ctl = w.open(None);
    let verify = ctl.effect(&w, "run_verification");
    assert_eq!((verify.state, verify.lease_generation), (EffectState::Dispatched, 2));

    // A zombie of the first attempt (lease 1) reports directly, and another left a forged
    // receipt in the executor's log.
    let zombie = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(1), b"{\"passed\": true}".to_vec());
    let before = (ctl.db.task(&w.task).unwrap(), ctl.db.usage_summary(&w.task).unwrap());
    let verdict = ctl.db.complete_effect(&verify.effect_id, &zombie.receipt, None, None).unwrap();
    assert_eq!(verdict, ReceiptVerdict::StaleLeaseIgnored);
    assert_eq!((ctl.db.task(&w.task).unwrap(), ctl.db.usage_summary(&w.task).unwrap()), before);
    let forged_out = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(1), b"{\"passed\": true}".to_vec());
    let forged = w.path("receipts").join(format!("{}-{}.json", verify.effect_id, forged_out.receipt.attempt_id));
    std::fs::write(&forged, serde_json::to_vec(&forged_out).unwrap()).unwrap();

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::Redispatch, "the stale receipt is not applied");
    let ignored = ctl.of_type(&w, "ReceiptIgnored");
    assert_eq!(ignored.len(), 2, "the direct report and the stale retained one");
    assert!(ignored.iter().all(|e| e["reason"] == "StaleLeaseIgnored"), "{ignored:?}");
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 3);

    // A duplicate of the applied receipt after the fact changes nothing either.
    let done = ctl.effect(&w, "run_verification");
    let applied = retained(&ctl, &done.effect_id);
    let verdict = ctl.db.complete_effect(&done.effect_id, &applied.receipt, done.result_digest.as_ref(), None).unwrap();
    assert_eq!(verdict, ReceiptVerdict::DuplicateIgnored);
    assert_eq!(ctl.of_type(&w, "ReceiptIgnored").len(), 3);
    assert_eq!(summarize(&w, &ctl), baseline);
    // An ignored retained receipt is audited once, however often recovery looks at it.
    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(ctl.events(&w).len(), n);
    assert_journal_sound(&w, &ctl);
}

/// Crashes at `point` in `kind`, has an operator request cancellation, then recovers.
async fn cancel_after_crash(point: CrashPoint, kind: &'static str) -> (World, Ctl, RecoveryReport) {
    let w = World::new();
    w.crash_run(CrashHook::at(point, kind)).await;
    let ctl = w.open(None);
    ctl.db.append(&w.task, &TaskEvent::CancelRequested).unwrap();
    let report = ctl.recover(&w).await;
    assert_eq!(report.state, Some(TaskState::Cancelled), "{report:?}");
    assert_eq!(ctl.db.task(&w.task).unwrap().state, TaskState::Cancelled);
    assert_eq!(ctl.of_type(&w, "CancelCompleted").len(), 1);
    // Nothing more happens on a further run.
    let n = ctl.events(&w).len();
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Cancelled);
    assert_eq!(ctl.events(&w).len(), n);
    assert_journal_sound(&w, &ctl);
    (w, ctl, report)
}

#[tokio::test]
async fn cancel_after_a_crash_abandons_a_patch_that_never_ran() {
    for point in [CrashPoint::AfterIntent, CrashPoint::AfterDispatch] {
        let (w, ctl, report) = cancel_after_crash(point, "apply_patch").await;
        assert_eq!(report.decisions[0].decision, Decision::Abandon, "{point}");
        let patch = ctl.effect(&w, "apply_patch");
        assert_eq!(patch.state, EffectState::Abandoned, "{point}");
        assert!(ctl.db.outstanding_effects(&w.task).unwrap().is_empty());
        assert_eq!(w.counts.get("apply_patch"), 0, "{point}");
        assert_eq!(workspace_digest(&w.ws()).unwrap(), w.base(), "{point}");
        let usage = ctl.db.usage_summary(&w.task).unwrap();
        assert_eq!(
            (usage.reserved_tool_actions, usage.settled_tool_actions, usage.uncertain_tool_actions),
            (0, 1, 0),
            "{point}: only the snapshot's action is consumed; the patch's is released"
        );
        assert_eq!(ctl.of_type(&w, "EffectAbandoned").len(), 1);
    }
}

#[tokio::test]
async fn cancel_after_a_crash_completes_a_retained_verification_without_success() {
    let (w, ctl, report) = cancel_after_crash(CrashPoint::AfterExecuteBeforePublish, "run_verification").await;
    assert_eq!(report.decisions[0].decision, Decision::PublishRetained);
    assert_eq!(ctl.effect(&w, "run_verification").state, EffectState::Completed);
    let rejected = ctl.of_type(&w, "TaskEventRejected");
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0]["event"].get("VerifyPassed").is_some(), "{rejected:?}");
    assert_eq!(ctl.db.task(&w.task).unwrap().verified_digest, None);
    assert_eq!(w.counts.get("run_verification"), 1);
}

#[tokio::test]
async fn cancel_after_a_crash_leaves_an_unprovable_verification_unknown() {
    let (w, ctl, report) = cancel_after_crash(CrashPoint::DuringExecute, "run_verification").await;
    assert_eq!(report.decisions[0].decision, Decision::MarkUnknown);
    let verify = ctl.effect(&w, "run_verification");
    assert_eq!(verify.state, EffectState::Unknown, "it ran and left no receipt: it may have happened");
    assert_eq!(ctl.db.outstanding_effects(&w.task).unwrap(), vec![verify]);
    assert_eq!(w.counts.get("run_verification"), 1, "not retried once cancel was requested");
    // Recovering again changes nothing.
    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(ctl.events(&w).len(), n);
}

#[tokio::test]
async fn an_unreconcilable_patch_fails_the_task_and_keeps_its_reservation_uncertain() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "apply_patch")).await;
    // Someone else touched the workspace too: it is neither the base nor base + patch.
    std::fs::write(w.ws().join("src/__init__.py"), "# changed by hand\n").unwrap();
    let ctl = w.open(None);

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions[0].decision, Decision::Unreconcilable);
    assert_eq!(report.state, Some(TaskState::Failed));
    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(patch.state, EffectState::Unknown);
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["Failed"]["reason"], format!("unreconcilable effect {}", patch.effect_id));
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.uncertain_tool_actions, usage.reserved_tool_actions, usage.settled_tool_actions), (1, 0, 1));
    assert_eq!(ctl.db.outstanding_effects(&w.task).unwrap(), vec![patch]);

    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Failed);
    assert_eq!(ctl.events(&w).len(), n, "a failed task is left alone");
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn a_workspace_lost_with_a_patch_in_flight_fails_the_task_cleanly() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch")).await;
    std::fs::remove_dir_all(w.ws()).unwrap();
    let ctl = w.open(None);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Failed);

    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(patch.state, EffectState::Unknown);
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed[0]["Failed"]["reason"], format!("unreconcilable effect {}", patch.effect_id));
    let n = ctl.events(&w).len();
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Failed);
    assert_eq!(ctl.events(&w).len(), n);
    assert_eq!(w.counts.get("apply_patch"), 0);
}

#[tokio::test]
async fn a_workspace_lost_between_effects_fails_the_task_cleanly() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterComplete, "apply_patch")).await;
    std::fs::remove_dir_all(w.ws()).unwrap();
    let ctl = w.open(None);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Failed);

    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert!(failed[0]["Failed"]["reason"].as_str().unwrap().starts_with("workspace lost"), "{failed:?}");
    assert_eq!(ctl.of_type(&w, "VerifyStarted").len(), 0, "no work on a workspace that is gone");
    let n = ctl.events(&w).len();
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Failed);
    assert_eq!(ctl.events(&w).len(), n);
}

#[tokio::test]
async fn a_paused_task_defers_recovery_of_work_it_cannot_dispatch() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterIntent, "apply_patch")).await;
    let ctl = w.open(None);
    ctl.db.append(&w.task, &TaskEvent::Paused).unwrap();

    let report = ctl.recover(&w).await;

    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(report.deferred, vec![patch.effect_id.clone()]);
    assert!(report.decisions.is_empty());
    assert_eq!(patch.state, EffectState::Intended);
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Paused);
    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 0);

    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    // A new session after the resume: the agent starts over from the current workspace.
    let mut agent = FakeAgent::scripted(vec![AgentAction::Verify, AgentAction::Finish]);
    let state = run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task).await.unwrap();
    assert_eq!(state, TaskState::Succeeded);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
    assert_eq!(w.counts.get("apply_patch"), 1);
    let task = ctl.db.task(&w.task).unwrap();
    assert_eq!(task.verified_digest, Some(workspace_digest(&w.ws()).unwrap()));
}

#[tokio::test]
async fn an_agent_that_replays_differently_fails_the_task() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterComplete, "apply_patch")).await;
    let ctl = w.open(None);
    let mut other = FakeAgent::scripted(vec![AgentAction::ApplyPatch(comment_patch())]);

    let err = run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut other, &w.task).await.unwrap_err();

    assert!(matches!(err, EngineError::NondeterministicAgent { turn: 1, .. }), "{err:?}");
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert!(failed[0]["Failed"]["reason"].as_str().unwrap().contains("replay"), "{failed:?}");
    assert_eq!(w.counts.get("apply_patch"), 1, "nothing was executed for the divergent agent");
}

#[tokio::test]
async fn replay_feeds_the_journaled_observations_and_executes_nothing_twice() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterAgentTurnJournaled, "run_verification")).await;
    let ctl = w.open(None);
    let turns = ctl.of_type(&w, "AgentTurn");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["action"], serde_json::json!("Verify"));
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task).await.unwrap(), TaskState::Succeeded);

    let journaled: Vec<_> =
        turns.iter().map(|t| serde_json::from_value(t["observation"].clone()).unwrap()).collect::<Vec<_>>();
    assert_eq!(agent.observations(), &journaled[..], "replayed exactly the journaled observations");
    assert_eq!(ctl.of_type(&w, "AgentTurn").len(), 2, "replayed turns are not journaled again");
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

/// Stand-in for the Phase 4 model broker: an effect holding a model-request reservation is
/// in flight when the controller dies, and nothing can tell whether the request was made.
#[tokio::test]
async fn during_model_call_the_reservation_stays_uncertain() {
    let w = World::new();
    let effect = {
        let ctl = w.open(None);
        ctl.db.append(&w.task, &TaskEvent::Started).unwrap();
        let kind = EffectKind::ExportBundle;
        let reserve = Reservation { tool_actions: 0, model_requests: 1 };
        let base = ctl.db.task(&w.task).unwrap().workspace_digest;
        let rec = ctl.db.record_intent(&w.task, kind, Digest::of(b"prompt"), &base, reserve).unwrap();
        ctl.db.mark_dispatched(&rec.effect_id, &AttemptId::new(), "model-broker", 1).unwrap();
        rec.effect_id
        // The controller dies with the model call in flight.
    };
    let ctl = w.open(None);

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::Unreconcilable, "ExportBundle cannot be retried blindly");
    let rec = ctl.db.effect(&effect).unwrap();
    assert_eq!(rec.state, EffectState::Unknown);
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!(
        (usage.uncertain_model_requests, usage.reserved_model_requests, usage.settled_model_requests),
        (1, 0, 0),
        "neither released nor doubled"
    );
    let journaled = ctl.of_type(&w, "RecoveryDecision");
    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0]["decision"], "Unreconcilable");
    assert_eq!(journaled[0]["found_state"], "Dispatched");
    assert!(journaled[0]["reason"].as_str().unwrap().contains("ReconcileThenRetry"), "{journaled:?}");
    assert_eq!(ctl.db.task(&w.task).unwrap().state, TaskState::Failed);

    let events = ctl.events(&w);
    let again = ctl.recover(&w).await;
    assert!(again.decisions.is_empty(), "{again:?}");
    assert_eq!(ctl.events(&w), events);
    assert_eq!(ctl.db.usage_summary(&w.task).unwrap(), usage);
}

#[tokio::test]
async fn the_executor_retains_the_highest_lease_outcome() {
    let w = World::new();
    let ctl = w.open(None);
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    let verify = ctl.effect(&w, "run_verification");
    let real = retained(&ctl, &verify.effect_id);
    assert_eq!(real.receipt.lease_generation, 1);
    assert_eq!(Some(Digest::of(&real.output)), verify.result_digest);
    assert_eq!(real.receipt.outcome, Outcome::Success);

    let newer = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(7), b"newer".to_vec());
    let older = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(0), b"older".to_vec());
    for out in [&newer, &older] {
        let name = format!("{}-{}.json", verify.effect_id, out.receipt.attempt_id);
        std::fs::write(w.path("receipts").join(name), serde_json::to_vec(out).unwrap()).unwrap();
    }
    let receipt: Receipt = retained(&ctl, &verify.effect_id).receipt;
    assert_eq!(receipt, newer.receipt);
}

/// A hook that never crashes but has an operator append `event` (from its own connection)
/// the first time the run passes `point` for `kind`: the interrupt lands after the runner's
/// own interrupt check and before its next write.
fn interrupt_at(w: &World, point: CrashPoint, kind: &'static str, event: TaskEvent) -> RunOptions {
    let operator = std::sync::Mutex::new(Db::open(&w.path("agentos.db")).unwrap());
    let task = w.task.clone();
    RunOptions::crash_with(CrashHook::new(move |p, ctx| {
        if p == point && ctx.kind == Some(kind) && ctx.occurrence == 0 {
            operator.lock().unwrap().append(&task, &event).unwrap();
        }
        false
    }))
}

/// Runs the fixture solution with an operator interrupt injected; it must end cleanly.
async fn interrupted_run(point: CrashPoint, kind: &'static str, event: TaskEvent) -> (World, Ctl, TaskState) {
    let w = World::new();
    let ctl = w.open(None);
    let opts = interrupt_at(&w, point, kind, event);
    let state = w.run(&ctl, &opts).await.unwrap_or_else(|e| panic!("{point} {kind}: the run must end cleanly, got {e:?}"));
    assert_journal_sound(&w, &ctl);
    (w, ctl, state)
}

async fn resume_with(w: &World, ctl: &Ctl, actions: Vec<AgentAction>) -> TaskState {
    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    let mut agent = FakeAgent::scripted(actions);
    run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task).await.unwrap()
}

#[tokio::test]
async fn a_pause_landing_after_the_intent_stops_the_run_and_resume_completes_it() {
    let (w, ctl, state) = interrupted_run(CrashPoint::AfterIntent, "apply_patch", TaskEvent::Paused).await;
    assert_eq!(state, TaskState::Paused);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Intended, "not dispatched while paused");
    assert_eq!(w.counts.get("apply_patch"), 0);

    // Resume: recovery dispatches the intended patch, a new session verifies it.
    assert_eq!(resume_with(&w, &ctl, vec![AgentAction::Verify, AgentAction::Finish]).await, TaskState::Succeeded);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn a_cancel_landing_after_the_intent_cancels_and_releases_the_intent() {
    let (w, ctl, state) = interrupted_run(CrashPoint::AfterIntent, "apply_patch", TaskEvent::CancelRequested).await;
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Abandoned);
    assert!(ctl.db.outstanding_effects(&w.task).unwrap().is_empty());
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.reserved_tool_actions, usage.settled_tool_actions), (0, 1));
    assert_eq!(workspace_digest(&w.ws()).unwrap(), w.base());
    assert!(ctl.recover(&w).await.decisions.is_empty());
}

#[tokio::test]
async fn a_pause_landing_after_a_journaled_patch_turn_stops_before_the_intent() {
    let baseline = baseline().await;
    let (w, ctl, state) = interrupted_run(CrashPoint::AfterAgentTurnJournaled, "apply_patch", TaskEvent::Paused).await;
    assert_eq!(state, TaskState::Paused);
    assert!(ctl.effects(&w).iter().all(|e| e.kind.tag() == "read_snapshot"), "no patch effect");

    // The resumed session starts over from Start and runs the whole solution once.
    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

#[tokio::test]
async fn a_cancel_landing_after_a_journaled_verify_turn_cancels_without_verifying() {
    let (w, ctl, state) = interrupted_run(CrashPoint::AfterAgentTurnJournaled, "run_verification", TaskEvent::CancelRequested).await;
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(ctl.of_type(&w, "VerifyStarted").len(), 0);
    assert!(ctl.effects(&w).iter().all(|e| e.kind.tag() != "run_verification"));
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
}

#[tokio::test]
async fn a_pause_landing_after_a_journaled_verify_turn_pauses_and_resume_verifies() {
    let (w, ctl, state) = interrupted_run(CrashPoint::AfterAgentTurnJournaled, "run_verification", TaskEvent::Paused).await;
    assert_eq!(state, TaskState::Paused);
    assert_eq!(ctl.of_type(&w, "VerifyStarted").len(), 0);

    assert_eq!(resume_with(&w, &ctl, vec![AgentAction::Verify]).await, TaskState::Succeeded);
    let task = ctl.db.task(&w.task).unwrap();
    assert_eq!(task.verified_digest, Some(workspace_digest(&w.ws()).unwrap()));
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}
