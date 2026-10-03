//! The model crash matrix. A controller driven by a `ModelAgent` over a `FakeProvider`
//! transcript is killed at every boundary of every effect kind (model calls, listings, reads
//! and the three job kinds), restarted over the same files, recovered, and run to the end:
//! the end state must equal the uncrashed run's, except for the documented forfeit of a lost
//! model response. A `ModelCall` is never re-dispatched (lease 1 forever) and the provider is
//! never sent a request twice for one reservation.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agentos_core::effect::{EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::ModelAgent;
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation};
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::recover::{recover, Decision, RecoveryReport};
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::runner::{run_task_with, EngineError};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use agentos_store::effects::UsageSummary;
use common::{
    contract_model, copy_dir, fixtures, processes_of_home, routing_over, scratch_root, supervised, transcript, worker_config,
    workspace_dir, EXIT_BEFORE_RECEIPT_ENV, TEST_WORKERS_ENV,
};
use serde_json::Value;
use tempfile::TempDir;

const TAGS: [&str; 6] = ["model_call", "list_files", "read_file", "read_snapshot", "apply_patch", "run_verification"];
const MODEL: &str = "claude-opus-5-5";
/// Sends of an uncrashed run of the `parser-fix-direct` transcript: list, read, patch, verify.
const BASELINE_SENDS: usize = 4;

/// Job kinds go to the supervised executor; `special` optionally routes one kind to a
/// differently configured one (the supervisor that exits before the receipt).
struct Jobs {
    plain: SupervisedExecutor,
    special: Option<(&'static str, SupervisedExecutor)>,
}

impl Executor for Jobs {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match &self.special {
            Some((tag, exec)) if *tag == req.kind.tag() => exec.run(req, ctx).await,
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

/// The routing executor, counting how often the shadow reader is run (`ExecCounts` counts
/// jobs and provider sends only).
struct Exec {
    inner: RoutingExecutor<Jobs>,
    reads: Arc<[AtomicUsize; 2]>,
}

impl Executor for Exec {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::ListFiles { .. } => self.reads[0].fetch_add(1, Ordering::SeqCst),
            EffectKind::ReadFile { .. } => self.reads[1].fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
        self.inner.run(req, ctx).await
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.inner.retained_outcome(effect)
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        self.inner.reconcile(req, ctx).await
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.inner.current_workspace(task)
    }

    async fn await_job(&self, effect: &EffectId) -> JobWait {
        self.inner.await_job(effect).await
    }

    async fn fence_job(&self, effect: &EffectId) -> bool {
        self.inner.fence_job(effect).await
    }
}

/// Everything on disk that survives a controller kill, plus the counters a restarted
/// controller shares with the test (executions, provider sends).
struct World {
    dir: TempDir,
    task: TaskId,
    contract: agentos_core::contract::Contract,
    counts: ExecCounts,
    reads: Arc<[AtomicUsize; 2]>,
    provider: FakeProvider,
}

/// The in-memory controller: dropped wholesale to simulate a kill.
struct Ctl {
    db: Db,
    blobs: BlobStore,
    exec: Exec,
}

impl World {
    fn new() -> World {
        let dir = scratch_root();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let (contract, digest) = contract_model(12, 10);
        let task = db.create_task(&contract, &digest).unwrap();
        db.approve_task(&task).unwrap();
        let provider = FakeProvider::from_file(&transcript("parser-fix-direct")).unwrap();
        World { dir, task, contract, counts: ExecCounts::default(), reads: Arc::default(), provider }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// A freshly restarted controller over this world's files. The hook goes to the job
    /// executor, the model executor and the shadow reader, so every `DuringExecute` is seen.
    fn open(&self, hook: Option<CrashHook>) -> Ctl {
        self.open_with(hook, None)
    }

    /// As [`open`], with jobs of `exit_before_receipt` running under a supervisor that exits
    /// after the worker's outcome and before the receipt.
    fn open_with(&self, hook: Option<CrashHook>, exit_before_receipt: Option<&'static str>) -> Ctl {
        let jobs = self.path("jobs");
        let host = worker_config(self.dir.path());
        let plain = supervised(&jobs, host.clone(), &self.counts, hook.clone(), &[]);
        let special = exit_before_receipt.map(|kind| {
            let env = [(TEST_WORKERS_ENV, "1"), (EXIT_BEFORE_RECEIPT_ENV, "1")];
            (kind, supervised(&jobs, host, &self.counts, hook.clone(), &env))
        });
        let inner = routing_over(
            self.dir.path(),
            Jobs { plain, special },
            Some(Box::new(self.provider.clone_handle())),
            &self.counts,
            hook,
        );
        Ctl {
            db: Db::open(&self.path("agentos.db")).unwrap(),
            blobs: BlobStore::open(self.path("blobs")).unwrap(),
            exec: Exec { inner, reads: self.reads.clone() },
        }
    }

    fn blobs_bytes(&self, digest: &Digest) -> Vec<u8> {
        BlobStore::open(self.path("blobs")).unwrap().get(digest).unwrap()
    }

    fn ws(&self) -> PathBuf {
        workspace_dir(self.dir.path(), &self.task)
    }

    /// Runs the task with a fresh `ModelAgent` (a restarted controller has nothing else).
    async fn run(&self, ctl: &Ctl, opts: &RunOptions) -> Result<TaskState, EngineError> {
        let mut agent = ModelAgent::new(self.contract.clone(), MODEL);
        run_task_with(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &self.task, opts).await
    }

    /// Runs until the hook fires, then drops the controller (the kill).
    async fn crash_run(&self, hook: CrashHook) -> CrashPoint {
        self.crash_run_with(hook, None).await
    }

    async fn crash_run_with(&self, hook: CrashHook, exit_before_receipt: Option<&'static str>) -> CrashPoint {
        let ctl = self.open_with(Some(hook.clone()), exit_before_receipt);
        match self.run(&ctl, &RunOptions::crash_with(hook)).await.unwrap_err() {
            EngineError::Crashed(p) => p,
            other => panic!("expected an injected crash, got {other:?}"),
        }
    }

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

    /// Executions of `tag`: provider sends for a model call, job launches, shadow reads.
    fn executions(&self, tag: &str) -> usize {
        match tag {
            "list_files" => self.reads[0].load(Ordering::SeqCst),
            "read_file" => self.reads[1].load(Ordering::SeqCst),
            _ => self.counts.get(tag),
        }
    }

    async fn assert_no_live_process(&self) {
        let started = Instant::now();
        loop {
            let live = processes_of_home(self.dir.path());
            if live.is_empty() {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "processes outlived recovery: {live:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Ctl {
    fn events(&self, w: &World) -> Vec<StoredEvent> {
        self.db.events(&w.task).unwrap()
    }

    fn of_type(&self, w: &World, event_type: &str) -> Vec<Value> {
        self.events(w).into_iter().filter(|e| e.event_type == event_type).map(|e| e.payload).collect()
    }

    /// Effects in intent order.
    fn effects(&self, w: &World) -> Vec<EffectRecord> {
        self.of_type(w, "EffectIntended")
            .into_iter()
            .map(|p| self.db.effect(&serde_json::from_value(p["effect_id"].clone()).unwrap()).unwrap())
            .collect()
    }

    fn effects_of(&self, w: &World, tag: &str) -> Vec<EffectRecord> {
        self.effects(w).into_iter().filter(|e| e.kind.tag() == tag).collect()
    }

    fn first_of(&self, w: &World, tag: &str) -> EffectRecord {
        self.effects_of(w, tag).into_iter().next().unwrap_or_else(|| panic!("no {tag} effect"))
    }

    async fn recover(&self, w: &World) -> RecoveryReport {
        recover(&self.db, &self.blobs, &self.exec, &w.task).await.unwrap()
    }

    /// The agent's observations, one per `AgentTurn`.
    fn observations(&self, w: &World) -> Vec<Value> {
        self.of_type(w, "AgentTurn").into_iter().map(|p| p["observation"].clone()).collect()
    }
}

/// The normalized end state a run must reach whatever crashed along the way.
#[derive(Debug, PartialEq)]
struct Summary {
    state: TaskState,
    task_workspace: Digest,
    ws_on_disk: Digest,
    verified: Option<Digest>,
    actions_used: u32,
    /// Every effect in intent order, by tag and final state.
    kinds: Vec<(&'static str, EffectState)>,
    usage: UsageSummary,
    blobs: BTreeSet<Digest>,
}

fn summarize(w: &World, ctl: &Ctl) -> Summary {
    let task = ctl.db.task(&w.task).unwrap();
    let referenced = ctl.db.referenced_blobs().unwrap().into_iter().collect::<BTreeSet<_>>();
    let on_disk = w.blobs_on_disk();
    assert_eq!(on_disk, referenced, "every stored blob is referenced and every referenced blob is stored");
    Summary {
        state: task.state,
        task_workspace: task.workspace_digest,
        ws_on_disk: workspace_digest(&w.ws()).unwrap(),
        verified: task.verified_digest,
        actions_used: task.actions_used,
        kinds: ctl.effects(w).iter().map(|e| (e.kind.tag(), e.state)).collect(),
        usage: ctl.db.usage_summary(&w.task).unwrap(),
        blobs: on_disk,
    }
}

impl Summary {
    /// The summary with the forfeited (FAILED) model calls removed and their number.
    fn without_forfeited(mut self) -> (Summary, usize) {
        let before = self.kinds.len();
        self.kinds.retain(|k| *k != ("model_call", EffectState::Failed));
        let removed = before - self.kinds.len();
        (self, removed)
    }
}

async fn baseline() -> Summary {
    let w = World::new();
    let ctl = w.open(None);
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    for (tag, n) in [("model_call", BASELINE_SENDS), ("list_files", 1), ("read_file", 1), ("apply_patch", 1), ("run_verification", 1), ("read_snapshot", 1)] {
        assert_eq!(w.executions(tag), n, "{tag}");
    }
    assert_eq!(w.provider.calls(), BASELINE_SENDS);
    let s = summarize(&w, &ctl);
    assert_eq!(s.state, TaskState::Succeeded);
    assert_eq!(s.verified, Some(s.ws_on_disk));
    assert_eq!(s.task_workspace, s.ws_on_disk);
    assert!(s.ws_on_disk.to_string().starts_with("060915ee"), "{}", s.ws_on_disk);
    assert_eq!(s.usage.settled_model_requests, BASELINE_SENDS as u64);
    assert_eq!(s.usage.uncertain_model_requests, 0);
    assert_eq!(s.usage.reserved_model_requests, 0);
    s
}

/// Journal invariants every run must keep, crashed or not: gapless sequence, no effect
/// completed twice, and no model call ever dispatched under a lease above 1.
fn assert_journal_sound(w: &World, ctl: &Ctl) {
    let events = ctl.events(w);
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>(), "gapless sequence");
    let mut completed = HashMap::new();
    for e in events.iter().filter(|e| e.event_type == "EffectCompleted" || e.event_type == "EffectFailed") {
        *completed.entry(e.payload["effect_id"].as_str().unwrap().to_string()).or_insert(0) += 1;
    }
    assert!(completed.values().all(|n| *n == 1), "an effect completed twice: {completed:?}");
    for e in events.iter().filter(|e| e.event_type == "EffectDispatched") {
        let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
        if ctl.db.effect(&id).unwrap().kind.tag() == "model_call" {
            assert_eq!(e.payload["lease_generation"], 1, "a model call was dispatched again: {e:?}");
        }
    }
    for rec in ctl.effects_of(w, "model_call") {
        assert_eq!(rec.lease_generation, 1, "{rec:?}");
    }
}

/// What a crash at `point` in an effect of `kind` must lead to.
#[derive(Debug)]
struct Expectation {
    /// The one decision recovery takes (none: nothing was outstanding).
    decision: Option<Decision>,
    /// Provider sends of the whole run.
    model_sends: usize,
    /// Lease generation of the first effect of the crashed kind.
    lease: u64,
    /// Forfeited `model_call` effects in the end state.
    extra_forfeited: usize,
    /// Executions of the crashed kind (sends for a model call).
    executions_of_kind: usize,
}

fn expected(point: CrashPoint, kind: &str) -> Expectation {
    use CrashPoint::*;
    match kind {
        "model_call" => {
            let decision = match point {
                AfterAgentTurnJournaled | AfterComplete => None,
                AfterIntent => Some(Decision::Dispatch),
                AfterDispatch | DuringExecute => Some(Decision::Forfeit),
                AfterExecuteBeforePublish | AfterBlobPut | AfterRegister => Some(Decision::PublishRetained),
            };
            let sends = if point == DuringExecute { BASELINE_SENDS + 1 } else { BASELINE_SENDS };
            let extra = usize::from(decision == Some(Decision::Forfeit));
            Expectation { decision, model_sends: sends, lease: 1, extra_forfeited: extra, executions_of_kind: sends }
        }
        "list_files" | "read_file" => {
            let (decision, executions) = match point {
                AfterAgentTurnJournaled | AfterComplete => (None, 1),
                AfterIntent => (Some(Decision::Dispatch), 1),
                AfterDispatch => (Some(Decision::Redispatch), 1),
                DuringExecute | AfterExecuteBeforePublish | AfterBlobPut | AfterRegister => (Some(Decision::Redispatch), 2),
            };
            let lease = if decision == Some(Decision::Redispatch) { 2 } else { 1 };
            Expectation { decision, model_sends: BASELINE_SENDS, lease, extra_forfeited: 0, executions_of_kind: executions }
        }
        _ => {
            // As crash_matrix.rs::expected_decision (no exit-before-receipt).
            let decision = match point {
                AfterAgentTurnJournaled | AfterComplete => None,
                AfterIntent => Some(Decision::Dispatch),
                AfterDispatch => Some(Decision::Redispatch),
                DuringExecute | AfterExecuteBeforePublish | AfterBlobPut | AfterRegister => Some(Decision::PublishRetained),
            };
            let lease = if decision == Some(Decision::Redispatch) { 2 } else { 1 };
            Expectation { decision, model_sends: BASELINE_SENDS, lease, extra_forfeited: 0, executions_of_kind: 1 }
        }
    }
}

/// Crashes at `point` in `kind`, restarts and recovers; returns the world, the restarted
/// controller, the recovery report and the effects outstanding before it.
async fn crash_and_recover(point: CrashPoint, kind: &'static str) -> (World, Ctl, RecoveryReport, Vec<EffectId>) {
    let w = World::new();
    assert_eq!(w.crash_run(CrashHook::at(point, kind)).await, point);
    let ctl = w.open(None);
    let outstanding = ctl.db.outstanding_effects(&w.task).unwrap().into_iter().map(|e| e.effect_id).collect();
    let report = ctl.recover(&w).await;
    (w, ctl, report, outstanding)
}

async fn crash_case(point: CrashPoint, kind: &'static str) {
    let baseline = baseline().await;
    let want = expected(point, kind);
    let w = World::new();
    assert_eq!(w.crash_run(CrashHook::at(point, kind)).await, point);

    let ctl = w.open(None);
    let outstanding: Vec<EffectId> = ctl.db.outstanding_effects(&w.task).unwrap().into_iter().map(|e| e.effect_id).collect();
    let referenced_before = ctl.db.referenced_blobs().unwrap();
    let leftovers: BTreeSet<_> = w.blobs_on_disk().into_iter().filter(|d| !referenced_before.contains(d)).collect();

    let report = ctl.recover(&w).await;

    assert!(report.gc_removed >= leftovers.len(), "{report:?} vs {leftovers:?}");
    for d in &referenced_before {
        assert!(w.blobs_on_disk().contains(d), "referenced blob {d} removed by gc");
    }
    let decided: Vec<_> = report.decisions.iter().map(|d| d.effect_id.clone()).collect();
    assert_eq!(decided, outstanding, "one decision per outstanding effect, in creation order");
    match want.decision {
        Some(decision) => {
            assert_eq!(report.decisions.len(), 1, "{report:?}");
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

    // Never NondeterministicAgent: the run succeeds.
    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded, "crash at {point} in {kind}");

    let (got, forfeited) = summarize(&w, &ctl).without_forfeited();
    let (mut want_summary, _) = baseline.without_forfeited();
    want_summary.usage.uncertain_model_requests = want.extra_forfeited as u64;
    assert_eq!(forfeited, want.extra_forfeited, "forfeited model calls");
    assert_eq!(got, want_summary, "crash at {point} in {kind}");
    assert_eq!(w.provider.calls(), want.model_sends, "provider sends");
    for tag in TAGS {
        let n = if tag == kind {
            want.executions_of_kind
        } else if tag == "model_call" {
            BASELINE_SENDS
        } else {
            1
        };
        assert_eq!(w.executions(tag), n, "executions of {tag}");
    }
    assert_eq!(w.counts.get("model_call"), w.provider.calls(), "every send was counted");
    assert_eq!(ctl.effects_of(&w, "model_call").len(), BASELINE_SENDS + want.extra_forfeited);
    assert_eq!(ctl.first_of(&w, kind).lease_generation, want.lease, "lease of the first {kind}");
    assert_journal_sound(&w, &ctl);
    assert_eq!(ctl.of_type(&w, "ReceiptIgnored").len(), 0);
    assert_eq!(ctl.of_type(&w, "EffectForfeited").len(), want.extra_forfeited);

    // Recovering a recovered, finished task changes nothing.
    let events = ctl.events(&w);
    let again = ctl.recover(&w).await;
    assert!(again.decisions.is_empty() && again.gc_removed == 0 && again.abandoned.is_empty(), "{again:?}");
    assert_eq!(ctl.events(&w), events);
    w.assert_no_live_process().await;
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
    model_call_after_agent_turn_journaled: AfterAgentTurnJournaled, "model_call";
    model_call_after_intent: AfterIntent, "model_call";
    model_call_after_dispatch: AfterDispatch, "model_call";
    model_call_during_execute: DuringExecute, "model_call";
    model_call_after_execute_before_publish: AfterExecuteBeforePublish, "model_call";
    model_call_after_blob_put: AfterBlobPut, "model_call";
    model_call_after_register: AfterRegister, "model_call";
    model_call_after_complete: AfterComplete, "model_call";
    list_files_after_agent_turn_journaled: AfterAgentTurnJournaled, "list_files";
    list_files_after_intent: AfterIntent, "list_files";
    list_files_after_dispatch: AfterDispatch, "list_files";
    list_files_during_execute: DuringExecute, "list_files";
    list_files_after_execute_before_publish: AfterExecuteBeforePublish, "list_files";
    list_files_after_blob_put: AfterBlobPut, "list_files";
    list_files_after_register: AfterRegister, "list_files";
    list_files_after_complete: AfterComplete, "list_files";
    read_file_after_agent_turn_journaled: AfterAgentTurnJournaled, "read_file";
    read_file_after_intent: AfterIntent, "read_file";
    read_file_after_dispatch: AfterDispatch, "read_file";
    read_file_during_execute: DuringExecute, "read_file";
    read_file_after_execute_before_publish: AfterExecuteBeforePublish, "read_file";
    read_file_after_blob_put: AfterBlobPut, "read_file";
    read_file_after_register: AfterRegister, "read_file";
    read_file_after_complete: AfterComplete, "read_file";
    // No read_snapshot row at AfterAgentTurnJournaled: the snapshot is taken by the runner before
    // the agent is asked (no agent turn precedes it), so that boundary is never passed for it.
    read_snapshot_after_intent: AfterIntent, "read_snapshot";
    read_snapshot_after_dispatch: AfterDispatch, "read_snapshot";
    read_snapshot_during_execute: DuringExecute, "read_snapshot";
    read_snapshot_after_execute_before_publish: AfterExecuteBeforePublish, "read_snapshot";
    read_snapshot_after_blob_put: AfterBlobPut, "read_snapshot";
    read_snapshot_after_register: AfterRegister, "read_snapshot";
    read_snapshot_after_complete: AfterComplete, "read_snapshot";
    apply_patch_after_agent_turn_journaled: AfterAgentTurnJournaled, "apply_patch";
    apply_patch_after_intent: AfterIntent, "apply_patch";
    apply_patch_after_dispatch: AfterDispatch, "apply_patch";
    apply_patch_during_execute: DuringExecute, "apply_patch";
    apply_patch_after_execute_before_publish: AfterExecuteBeforePublish, "apply_patch";
    apply_patch_after_blob_put: AfterBlobPut, "apply_patch";
    apply_patch_after_register: AfterRegister, "apply_patch";
    apply_patch_after_complete: AfterComplete, "apply_patch";
    run_verification_after_agent_turn_journaled: AfterAgentTurnJournaled, "run_verification";
    run_verification_after_intent: AfterIntent, "run_verification";
    run_verification_after_dispatch: AfterDispatch, "run_verification";
    run_verification_during_execute: DuringExecute, "run_verification";
    run_verification_after_execute_before_publish: AfterExecuteBeforePublish, "run_verification";
    run_verification_after_blob_put: AfterBlobPut, "run_verification";
    run_verification_after_register: AfterRegister, "run_verification";
    run_verification_after_complete: AfterComplete, "run_verification";
}

#[tokio::test]
async fn baseline_model_run_succeeds_with_four_sends() {
    let s = baseline().await;
    assert_eq!(s.kinds.iter().filter(|k| k.0 == "model_call").count(), 4);
    assert!(s.kinds.iter().all(|k| k.1 == EffectState::Completed));
}

#[tokio::test]
async fn every_model_row_replays_without_divergence_and_never_redispatches_a_model_call() {
    for point in CrashPoint::ALL {
        let (w, ctl, report, _) = crash_and_recover(point, "model_call").await;
        assert!(report.decisions.iter().all(|d| d.decision != Decision::Redispatch), "{point}: {report:?}");
        let state = w.run(&ctl, &RunOptions::default()).await;
        assert!(
            !matches!(state, Err(EngineError::NondeterministicAgent { .. })),
            "{point}: the replay diverged: {state:?}"
        );
        assert_eq!(state.unwrap(), TaskState::Succeeded, "{point}");
        let calls = ctl.effects_of(&w, "model_call");
        assert!(calls.iter().all(|r| r.lease_generation == 1), "{point}: {calls:?}");
        assert_journal_sound(&w, &ctl);
        assert!(w.provider.calls() <= BASELINE_SENDS + 1, "{point}: {} sends", w.provider.calls());
    }
}

#[tokio::test]
async fn model_after_dispatch_forfeits_then_asks_again() {
    let (w, ctl, report, outstanding) = crash_and_recover(CrashPoint::AfterDispatch, "model_call").await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::Forfeit);
    assert_eq!(w.provider.calls(), 0, "the call never reached the provider, and recovery sent nothing");
    let lost = ctl.db.effect(&outstanding[0]).unwrap();
    assert_eq!((lost.state, lost.result_digest, lost.lease_generation), (EffectState::Failed, None, 1));
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.uncertain_model_requests, usage.reserved_model_requests, usage.settled_model_requests), (1, 0, 0));

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    let calls = ctl.effects_of(&w, "model_call");
    assert_eq!(calls.len(), 5, "one forfeited, four completed");
    assert_eq!((calls[0].state, calls[0].result_digest), (EffectState::Failed, None));
    assert!(calls[1..].iter().all(|r| r.state == EffectState::Completed && r.lease_generation == 1), "{calls:?}");
    assert_eq!(calls[0].request_digest, calls[1].request_digest, "the same body is asked again");
    assert_ne!(calls[0].effect_id, calls[1].effect_id, "under a new effect and reservation");
    let turns: Vec<u32> = calls
        .iter()
        .map(|r| match r.kind {
            EffectKind::ModelCall { turn, .. } => turn,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(turns[..2], [1, 2], "the next turn gets a new effect");
    assert!(ctl.observations(&w).contains(&serde_json::json!("ModelCallLost")), "{:?}", ctl.observations(&w));
    assert_eq!(w.provider.calls(), 4);
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.uncertain_model_requests, usage.settled_model_requests, usage.reserved_model_requests), (1, 4, 0));
    assert_eq!(ctl.of_type(&w, "EffectForfeited").len(), 1);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn model_during_execute_forfeits_and_counts_the_lost_send_as_uncertain() {
    let (w, ctl, report, outstanding) = crash_and_recover(CrashPoint::DuringExecute, "model_call").await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::Forfeit);
    assert_eq!(w.provider.calls(), 1, "the lost call did reach the provider; recovery sent nothing more");
    let lost = ctl.db.effect(&outstanding[0]).unwrap();
    assert_eq!((lost.state, lost.result_digest, lost.lease_generation), (EffectState::Failed, None, 1));
    assert_eq!(ctl.db.usage_summary(&w.task).unwrap().uncertain_model_requests, 1);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    assert_eq!(w.provider.calls(), 5, "the lost send plus the four of the run");
    assert_eq!(w.counts.get("model_call"), 5);
    let calls = ctl.effects_of(&w, "model_call");
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[0].effect_id, lost.effect_id);
    assert_eq!(calls[0].state, EffectState::Failed);
    assert!(calls[1..].iter().all(|r| r.state == EffectState::Completed && r.lease_generation == 1), "{calls:?}");
    assert_eq!(
        w.blobs_bytes(&calls[0].request_digest),
        w.blobs_bytes(&calls[1].request_digest),
        "the two first bodies are byte-equal"
    );
    assert_ne!(calls[0].effect_id, calls[1].effect_id, "ids differ");
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!((usage.uncertain_model_requests, usage.settled_model_requests, usage.reserved_model_requests), (1, 4, 0));
    assert!(ctl.observations(&w).contains(&serde_json::json!("ModelCallLost")));
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn model_after_execute_before_publish_publishes_the_retained_response_without_a_second_send() {
    let (w, ctl, report, outstanding) = crash_and_recover(CrashPoint::AfterExecuteBeforePublish, "model_call").await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(report.decisions[0].decision, Decision::PublishRetained);
    assert_eq!(w.provider.calls(), 1, "the one send before the crash");
    let retained = ctl.exec.retained_outcome(&outstanding[0]).expect("the response was retained before the crash");
    let dir = w.path("model");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    assert_eq!(files.len(), 1, "one retention directory");
    let on_disk: ExecOutcome = serde_json::from_slice(&std::fs::read(files[0].path().join("response.json")).unwrap()).unwrap();
    assert_eq!(on_disk.output, retained.output);

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    assert_eq!(w.provider.calls(), BASELINE_SENDS, "no second send of the retained call");
    let first = ctl.db.effect(&outstanding[0]).unwrap();
    assert_eq!((first.state, first.lease_generation), (EffectState::Completed, 1));
    assert_eq!(w.blobs_bytes(&first.result_digest.unwrap()), on_disk.output, "the retained response is the published blob");
    assert_eq!(ctl.effects_of(&w, "model_call").len(), BASELINE_SENDS);
    assert_eq!(ctl.db.usage_summary(&w.task).unwrap().uncertain_model_requests, 0);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn reads_crashed_after_intent_complete_with_the_same_bytes() {
    let base = World::new();
    let bctl = base.open(None);
    assert_eq!(base.run(&bctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);
    for kind in ["list_files", "read_file"] {
        let (w, ctl, report, _) = crash_and_recover(CrashPoint::AfterIntent, kind).await;
        assert_eq!(report.decisions[0].decision, Decision::Dispatch, "{kind}");
        assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded, "{kind}");
        let (got, want) = (ctl.first_of(&w, kind), bctl.first_of(&base, kind));
        assert_eq!((got.state, got.lease_generation), (EffectState::Completed, 1), "{kind}");
        assert_eq!(w.blobs_bytes(&got.result_digest.unwrap()), base.blobs_bytes(&want.result_digest.unwrap()), "{kind}: same bytes");
        assert_eq!(ctl.effects_of(&w, kind).len(), 1);
        assert_eq!(w.executions(kind), 1, "{kind}");
        assert_journal_sound(&w, &ctl);
    }
}

/// The controller dies with the patch job running, and the job's supervisor dies after the
/// patch was applied and before its receipt: with a model agent driving the run, recovery
/// still reconciles it (applied once, never twice).
#[tokio::test]
async fn a_crash_during_the_patch_job_still_reconciles_with_a_model_agent() {
    let baseline = baseline().await;
    let w = World::new();
    assert_eq!(
        w.crash_run_with(CrashHook::at(CrashPoint::DuringExecute, "apply_patch"), Some("apply_patch")).await,
        CrashPoint::DuringExecute
    );
    let ctl = w.open(None);
    let report = ctl.recover(&w).await;
    assert_eq!(report.decisions.len(), 1, "{report:?}");
    assert_eq!(report.decisions[0].decision, Decision::PublishReconciled, "{report:?}");
    assert_eq!(report.decisions[0].kind, "apply_patch");

    assert_eq!(w.run(&ctl, &RunOptions::default()).await.unwrap(), TaskState::Succeeded);

    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.executions("apply_patch"), 1, "the patch ran once");
    assert_eq!(w.provider.calls(), BASELINE_SENDS);
    assert_eq!(ctl.effects_of(&w, "model_call").iter().filter(|r| r.lease_generation != 1).count(), 0);
    assert_eq!(ctl.of_type(&w, "ReceiptIgnored").len(), 0);
    assert_journal_sound(&w, &ctl);
    w.assert_no_live_process().await;
}
