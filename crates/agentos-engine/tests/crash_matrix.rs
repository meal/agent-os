//! Crash injection and recovery. Every test simulates a controller kill by returning
//! `EngineError::Crashed` at a boundary and then dropping every in-memory handle (database,
//! blob store, executor, agent); the restarted controller reopens the same on-disk paths
//! with fresh objects. Effects run as supervised jobs (the real `agentos-supervisor`), which
//! outlive the "killed" controller exactly as they would a real one.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::broker::Resource;
use agentos_core::budget::Reservation;
use agentos_core::effect::{
    AttemptId, EffectId, EffectKind, EffectRecord, EffectState, Outcome, Receipt, ReceiptVerdict,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::lease::EffectTimeouts;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{AgentAction, FakeAgent};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{
    AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation,
};
use agentos_engine::job::{JobDir, JobRequest, JobState, ScriptedConfig, WorkerConfig};
use agentos_engine::recover::{Decision, RecoveryReport, recover, recover_with};
use agentos_engine::runner::{EngineError, run_task, run_task_with};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_engine::supervisor::POLL_ENV;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use agentos_store::effects::UsageSummary;
use common::{
    EXIT_BEFORE_RECEIPT_ENV, TEST_WORKERS_ENV, all_pids, comment_patch, contract, copy_dir,
    firecracker_processes, fix_patch, fixtures, lose_workspace, proc_state, processes_naming,
    processes_of_home, real_mode, supervised, worker_config, workspace_dir, write_in_workspace,
};
use rustix::process::{Pid, PidfdFlags, Signal, kill_process, pidfd_open, pidfd_send_signal};
use tempfile::TempDir;

const KINDS: [&str; 3] = ["read_snapshot", "apply_patch", "run_verification"];
/// Upper bound for anything a test waits on outside the engine.
const PATIENCE: Duration = Duration::from_secs(20);

/// Everything on disk that survives a controller kill.
struct World {
    dir: TempDir,
    task: TaskId,
    counts: ExecCounts,
}

/// How a controller's executor is put together.
#[derive(Debug, Clone, Default)]
struct ExecOpts {
    /// Effects of this kind run under a supervisor that exits after the worker's outcome and
    /// before the receipt (the Phase 2 "executed but not durable" case).
    exit_before_receipt: Option<&'static str>,
    /// Verifications run this shell script (a scripted worker) instead of the profile check.
    scripted_verification: Option<String>,
    /// Effects of this kind run under a supervisor that polls only every 10 s, so it notices
    /// nothing (the worker's exit, its lease) for that long.
    slow_poll: Option<&'static str>,
    timeouts: Option<EffectTimeouts>,
    fence: Fence,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Fence {
    #[default]
    Real,
    /// `fence_job` fails without stopping anything.
    Fails,
    /// `fence_job` never returns: the controller dies while fencing.
    Hangs,
}

/// The controller's executor: supervised jobs, with one kind optionally routed to a
/// differently configured supervised executor over the same jobs root and counts.
struct Exec {
    plain: SupervisedExecutor,
    special: Option<(&'static str, SupervisedExecutor)>,
    fence: Fence,
}

impl Executor for Exec {
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
        match self.fence {
            Fence::Real => self.plain.fence_job(effect).await,
            Fence::Fails => false,
            Fence::Hangs => std::future::pending().await,
        }
    }
}

/// The in-memory controller: dropped wholesale to simulate a kill.
struct Ctl {
    db: Db,
    blobs: BlobStore,
    exec: Exec,
}

impl World {
    fn new() -> World {
        let dir = common::scratch_root();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let (contract, digest) = contract(10);
        let task = db.create_task(&contract, &digest).unwrap();
        db.approve_task(&task).unwrap();
        World {
            dir,
            task,
            counts: ExecCounts::default(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn exec(&self, hook: Option<CrashHook>, opts: &ExecOpts) -> Exec {
        let jobs = self.path("jobs");
        let host = worker_config(self.dir.path());
        let timeouts = opts.timeouts.unwrap_or_default();
        let plain = supervised(&jobs, host.clone(), &self.counts, hook.clone(), &[])
            .with_timeouts(timeouts);
        let special = match (&opts.scripted_verification, opts.exit_before_receipt) {
            (None, None) if opts.slow_poll.is_some() => {
                let env = [(TEST_WORKERS_ENV, "1"), (POLL_ENV, "10000")];
                Some((
                    opts.slow_poll.unwrap(),
                    supervised(&jobs, host, &self.counts, hook, &env).with_timeouts(timeouts),
                ))
            }
            (Some(script), _) => {
                let worker = WorkerConfig::Scripted(ScriptedConfig {
                    script: script.clone(),
                });
                let exec = supervised(
                    &jobs,
                    worker,
                    &self.counts,
                    hook,
                    &[(TEST_WORKERS_ENV, "1")],
                );
                Some(("run_verification", exec.with_timeouts(timeouts)))
            }
            (None, Some(kind)) => {
                let env = [(TEST_WORKERS_ENV, "1"), (EXIT_BEFORE_RECEIPT_ENV, "1")];
                Some((
                    kind,
                    supervised(&jobs, host, &self.counts, hook, &env).with_timeouts(timeouts),
                ))
            }
            (None, None) => None,
        };
        Exec {
            plain,
            special,
            fence: opts.fence,
        }
    }

    /// A freshly restarted controller over this world's files.
    fn open(&self, hook: Option<CrashHook>) -> Ctl {
        self.open_with(hook, &ExecOpts::default())
    }

    fn open_with(&self, hook: Option<CrashHook>, opts: &ExecOpts) -> Ctl {
        Ctl {
            db: Db::open(&self.path("agentos.db")).unwrap(),
            blobs: BlobStore::open(self.path("blobs")).unwrap(),
            exec: self.exec(hook, opts),
        }
    }

    fn ws(&self) -> PathBuf {
        workspace_dir(self.dir.path(), &self.task)
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
                let hex = format!(
                    "{}{}",
                    shard.file_name().to_str().unwrap(),
                    obj.unwrap().file_name().to_str().unwrap()
                );
                out.insert(Digest::from_hex(&hex).unwrap());
            }
        }
        out
    }

    fn counts(&self) -> HashMap<&'static str, usize> {
        KINDS.iter().map(|k| (*k, self.counts.get(k))).collect()
    }

    /// Every job directory of `effect`, ascending by lease generation.
    fn jobs(&self, effect: &EffectId) -> Vec<JobDir> {
        JobDir::list(&self.path("jobs"), effect).unwrap()
    }

    fn only_job(&self, effect: &EffectId) -> JobDir {
        let mut jobs = self.jobs(effect);
        assert_eq!(jobs.len(), 1, "one job for {effect}");
        jobs.remove(0)
    }

    /// Runs the scripted fixture solution with a fresh agent.
    async fn run(&self, ctl: &Ctl, opts: &RunOptions) -> Result<TaskState, EngineError> {
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        run_task_with(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &self.task, opts).await
    }

    /// Runs until the hook fires, then drops the controller (the kill).
    async fn crash_run(&self, hook: CrashHook) -> CrashPoint {
        self.crash_run_with(hook, &ExecOpts::default()).await
    }

    async fn crash_run_with(&self, hook: CrashHook, opts: &ExecOpts) -> CrashPoint {
        let ctl = self.open_with(Some(hook.clone()), opts);
        let err = self
            .run(&ctl, &RunOptions::crash_with(hook))
            .await
            .unwrap_err();
        match err {
            EngineError::Crashed(p) => p,
            other => panic!("expected an injected crash, got {other:?}"),
        }
    }

    /// Points the profile (the world's copy) at the real check, run after `secs` seconds.
    fn slow_profile(&self, secs: f64) {
        let script = format!("sleep {secs}; exec python3 check_parser.py \"$1\"");
        let profile = serde_json::json!({ "id": "parser-checks-v1", "command": ["sh", "-c", script, "sh"], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    fn restore_profile(&self) {
        fs::copy(
            fixtures().join("profiles/parser-checks-v1/profile.json"),
            self.path("profile/profile.json"),
        )
        .unwrap();
    }

    /// Live (non-zombie) processes whose command line names anything in this world: the
    /// supervisors and workers (their job directory) and the checks (the workspace).
    fn live_processes(&self) -> Vec<i32> {
        processes_of_home(self.dir.path())
    }

    /// Waits for every process of this world to be gone; panics with the survivors.
    async fn assert_no_live_process(&self) {
        let started = Instant::now();
        loop {
            let live = self.live_processes();
            if live.is_empty() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "processes outlived recovery: {live:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Live (non-zombie) processes in session `sid`.
fn session_members(sid: i32) -> Vec<i32> {
    all_pids()
        .into_iter()
        .filter(|pid| proc_state(*pid).is_some_and(|(s, _, sess)| sess == sid && s != "Z"))
        .collect()
}

/// Live (non-zombie) processes in process group `pgid`.
fn group_members(pgid: i32) -> Vec<i32> {
    all_pids()
        .into_iter()
        .filter(|pid| proc_state(*pid).is_some_and(|(s, grp, _)| grp == pgid && s != "Z"))
        .collect()
}

/// No live process is left in the session of any job's recorded supervisor, nor in its
/// recorded worker group or `groups`.
fn assert_job_sessions_empty(w: &World) {
    for entry in fs::read_dir(w.path("jobs")).unwrap().flatten() {
        let job = JobDir::open(&entry.path()).unwrap();
        let Some(status) = job.read_status() else {
            continue;
        };
        if let Some(pid) = status.supervisor_pid {
            assert_eq!(
                session_members(pid as i32),
                Vec::<i32>::new(),
                "session of {}",
                job.path.display()
            );
        }
        for pgid in status.worker_pgid.into_iter().chain(job.groups()) {
            assert_eq!(
                group_members(pgid),
                Vec::<i32>::new(),
                "group {pgid} of {}",
                job.path.display()
            );
        }
    }
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// SIGSTOPs a process and SIGKILLs it when dropped, so a failing test never leaves a
/// stopped supervisor behind. Both go through a pidfd opened before the stop, so a pid
/// reaped and reused meanwhile is never hit.
struct Stopped(OwnedFd);

impl Stopped {
    fn stop(pid: i32) -> Stopped {
        let fd = pidfd_open(Pid::from_raw(pid).unwrap(), PidfdFlags::empty()).unwrap();
        pidfd_send_signal(&fd, Signal::STOP).unwrap();
        Stopped(fd)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = pidfd_send_signal(&self.0, Signal::KILL);
    }
}

/// Whether `job`'s check (or script) has started: the host and scripted workers record its
/// group in `groups`; the fake guest runs it in a group of its own that the worker never
/// sees, so there it is a live process whose command line names the guest's workspace (the
/// check's last argument).
fn check_started(job: &JobDir) -> bool {
    if !job.groups().is_empty() {
        return true;
    }
    match job.request().map(|r| (r.worker, r.task_id, r.attempt_id)) {
        // The real guest's check is invisible to the host: its VM is the proxy (the check
        // starts within a second of the boot; the slow profiles sleep for longer than that).
        Ok((WorkerConfig::Firecracker(_), _, attempt)) if real_mode() => {
            let id = attempt.to_string();
            firecracker_processes()
                .iter()
                .any(|p| p.id() == Some(id.as_str()))
        }
        Ok((WorkerConfig::Firecracker(cfg), task, _)) => {
            !processes_naming(&cfg.work_root.join(task.as_str()).join("workspace")).is_empty()
        }
        _ => false,
    }
}

/// The supervisor pid of a running `job`, once its check (or script) has started.
async fn running_supervisor(job: &JobDir) -> i32 {
    wait_until("the job's check to run", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Running)
            && check_started(job)
    })
    .await;
    job.read_status().unwrap().supervisor_pid.unwrap() as i32
}

impl Ctl {
    fn events(&self, w: &World) -> Vec<StoredEvent> {
        self.db.events(&w.task).unwrap()
    }

    fn of_type(&self, w: &World, event_type: &str) -> Vec<serde_json::Value> {
        self.events(w)
            .into_iter()
            .filter(|e| e.event_type == event_type)
            .map(|e| e.payload)
            .collect()
    }

    /// Effects in intent order.
    fn effects(&self, w: &World) -> Vec<EffectRecord> {
        self.of_type(w, "EffectIntended")
            .into_iter()
            .map(|p| {
                self.db
                    .effect(&serde_json::from_value(p["effect_id"].clone()).unwrap())
                    .unwrap()
            })
            .collect()
    }

    fn effect(&self, w: &World, tag: &str) -> EffectRecord {
        let mut found: Vec<_> = self
            .effects(w)
            .into_iter()
            .filter(|e| e.kind.tag() == tag)
            .collect();
        assert_eq!(found.len(), 1, "exactly one {tag} effect");
        found.remove(0)
    }

    async fn recover(&self, w: &World) -> RecoveryReport {
        recover(&self.db, &self.blobs, &self.exec, &w.task)
            .await
            .unwrap()
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
    let referenced = ctl
        .db
        .referenced_blobs()
        .unwrap()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let on_disk = w.blobs_on_disk();
    assert_eq!(
        on_disk, referenced,
        "every stored blob is referenced and every referenced blob is stored"
    );
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
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
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
    assert_eq!(
        seqs,
        (1..=events.len() as u64).collect::<Vec<_>>(),
        "gapless sequence"
    );
    let mut completed = HashMap::new();
    for e in events
        .iter()
        .filter(|e| e.event_type == "EffectCompleted" || e.event_type == "EffectFailed")
    {
        *completed
            .entry(e.payload["effect_id"].as_str().unwrap().to_string())
            .or_insert(0) += 1;
    }
    assert!(
        completed.values().all(|n| *n == 1),
        "an effect completed twice: {completed:?}"
    );
}

/// The decision recovery takes after a crash at `point` in `kind`. With `exit_before_receipt`
/// the job's supervisor also died after the worker finished and before the receipt.
///
/// `DuringExecute` now means the controller died with the job running: the job finishes on
/// its own and recovery waits for it and publishes its receipt (Phase 2 re-ran it, or
/// reconciled the patch, because its receipt was lost with the controller). The Phase 2
/// "executed but not durable" case is the exit-before-receipt one.
fn expected_decision(point: CrashPoint, kind: &str, exit_before_receipt: bool) -> Option<Decision> {
    match point {
        CrashPoint::AfterIntent => Some(Decision::Dispatch),
        CrashPoint::AfterDispatch => Some(Decision::Redispatch),
        CrashPoint::DuringExecute if !exit_before_receipt => Some(Decision::PublishRetained),
        CrashPoint::DuringExecute if kind == "apply_patch" => Some(Decision::PublishReconciled),
        CrashPoint::DuringExecute => Some(Decision::Redispatch),
        CrashPoint::AfterExecuteBeforePublish
        | CrashPoint::AfterBlobPut
        | CrashPoint::AfterRegister => Some(Decision::PublishRetained),
        CrashPoint::AfterComplete | CrashPoint::AfterAgentTurnJournaled => None,
    }
}

/// Real executions (job launches) of `tag` once a crash at `point` in `kind` was recovered.
/// Only an attempt that ran without leaving a durable receipt may run again, and only when
/// its kind is safe to retry blindly; the patch is reconciled instead.
fn expected_executions(
    point: CrashPoint,
    kind: &str,
    tag: &str,
    exit_before_receipt: bool,
) -> usize {
    let rerun = exit_before_receipt
        && point == CrashPoint::DuringExecute
        && kind == tag
        && kind != "apply_patch";
    if rerun { 2 } else { 1 }
}

async fn crash_case(point: CrashPoint, kind: &'static str) {
    crash_case_with(point, kind, false).await;
}

async fn crash_case_with(point: CrashPoint, kind: &'static str, exit_before_receipt: bool) {
    let baseline = baseline().await;
    let w = World::new();
    let opts = ExecOpts {
        exit_before_receipt: exit_before_receipt.then_some(kind),
        ..ExecOpts::default()
    };
    assert_eq!(
        w.crash_run_with(CrashHook::at(point, kind), &opts).await,
        point
    );

    let ctl = w.open(None);
    let outstanding: Vec<EffectId> = ctl
        .db
        .outstanding_effects(&w.task)
        .unwrap()
        .into_iter()
        .map(|e| e.effect_id)
        .collect();
    let referenced_before = ctl.db.referenced_blobs().unwrap();
    let leftovers: BTreeSet<_> = w
        .blobs_on_disk()
        .into_iter()
        .filter(|d| !referenced_before.contains(d))
        .collect();
    if point == CrashPoint::AfterBlobPut {
        assert!(
            !leftovers.is_empty(),
            "the crash left an unregistered blob behind"
        );
    }

    let report = ctl.recover(&w).await;

    assert!(
        report.gc_removed >= leftovers.len(),
        "{report:?} vs {leftovers:?}"
    );
    for d in &leftovers {
        assert!(
            !w.blobs_on_disk().contains(d) || ctl.db.referenced_blobs().unwrap().contains(d),
            "leftover {d} kept"
        );
    }
    for d in &referenced_before {
        assert!(
            w.blobs_on_disk().contains(d),
            "referenced blob {d} removed by gc"
        );
    }
    let decided: Vec<_> = report
        .decisions
        .iter()
        .map(|d| d.effect_id.clone())
        .collect();
    assert_eq!(
        decided, outstanding,
        "one decision per outstanding effect, in creation order"
    );
    let expected = expected_decision(point, kind, exit_before_receipt);
    match expected {
        Some(decision) => {
            assert_eq!(report.decisions.len(), 1);
            assert_eq!(report.decisions[0].decision, decision, "{report:?}");
            assert_eq!(report.decisions[0].kind, kind);
            let journaled = ctl.of_type(&w, "RecoveryDecision");
            assert_eq!(journaled.len(), 1);
            assert_eq!(
                journaled[0]["decision"],
                serde_json::to_value(decision).unwrap()
            );
            assert_eq!(
                journaled[0]["effect_id"],
                serde_json::to_value(&outstanding[0]).unwrap()
            );
        }
        None => assert!(
            report.decisions.is_empty() && outstanding.is_empty(),
            "{report:?}"
        ),
    }
    assert!(ctl.db.outstanding_effects(&w.task).unwrap().is_empty());
    assert_eq!(
        workspace_digest(&w.ws()).unwrap(),
        ctl.db.task(&w.task).unwrap().workspace_digest,
        "journal and workspace agree"
    );

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );

    assert_eq!(summarize(&w, &ctl), baseline, "crash at {point} in {kind}");
    for tag in KINDS {
        assert_eq!(
            w.counts.get(tag),
            expected_executions(point, kind, tag, exit_before_receipt),
            "executions of {tag}"
        );
    }
    let lease = if expected == Some(Decision::Redispatch) {
        2
    } else {
        1
    };
    assert_eq!(ctl.effect(&w, kind).lease_generation, lease);
    assert_journal_sound(&w, &ctl);
    assert_eq!(ctl.of_type(&w, "ReceiptIgnored").len(), 0);

    // Recovering a recovered, finished task changes nothing.
    let events = ctl.events(&w);
    let again = ctl.recover(&w).await;
    assert!(
        again.decisions.is_empty() && again.gc_removed == 0 && again.abandoned.is_empty(),
        "{again:?}"
    );
    assert_eq!(ctl.events(&w), events);
    assert_eq!(summarize(&w, &ctl), baseline);
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

/// The controller dies with the patch job running, and the job's supervisor dies after the
/// patch was applied and before its receipt: nothing durable says it happened, so recovery
/// reconciles it (applied once, never twice).
#[tokio::test]
async fn patch_supervisor_exits_before_receipt() {
    crash_case_with(CrashPoint::DuringExecute, "apply_patch", true).await;
}

/// As above for a verification: it is safe to run again, so it is (lease 2).
#[tokio::test]
async fn verification_supervisor_exits_before_receipt() {
    crash_case_with(CrashPoint::DuringExecute, "run_verification", true).await;
}

#[tokio::test]
async fn baseline_is_reproducible_across_worlds() {
    assert_eq!(baseline().await, baseline().await);
}

#[tokio::test]
async fn run_task_recovers_outstanding_effects_by_itself() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterRegister, "apply_patch"))
        .await;
    let ctl = w.open(None);

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );

    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 1);
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

#[tokio::test]
async fn recovering_twice_before_resuming_is_idempotent() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch"))
        .await;
    let ctl = w.open(None);
    let first = ctl.recover(&w).await;
    assert_eq!(first.decisions.len(), 1);
    let (events, effects, usage, blobs) = (
        ctl.events(&w),
        ctl.effects(&w),
        ctl.db.usage_summary(&w.task).unwrap(),
        w.blobs_on_disk(),
    );

    let second = recover(&ctl.db, &ctl.blobs, &ctl.exec, &w.task)
        .await
        .unwrap();

    assert!(
        second.decisions.is_empty() && second.gc_removed == 0,
        "{second:?}"
    );
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
    let hooked = ExecOpts {
        exit_before_receipt: Some("apply_patch"),
        ..ExecOpts::default()
    };
    w.crash_run_with(
        CrashHook::at(CrashPoint::DuringExecute, "apply_patch"),
        &hooked,
    )
    .await;
    {
        // The first recovery reconciles the patch, then dies before completing it.
        let hook = CrashHook::at(CrashPoint::AfterRegister, "apply_patch");
        let ctl = w.open(Some(hook.clone()));
        let err = recover_with(
            &ctl.db,
            &ctl.blobs,
            &ctl.exec,
            &w.task,
            &RunOptions::crash_with(hook),
        )
        .await;
        assert!(
            matches!(err, Err(EngineError::Crashed(CrashPoint::AfterRegister))),
            "{err:?}"
        );
    }
    let ctl = w.open(None);
    let report = ctl.recover(&w).await;
    assert_eq!(report.decisions.len(), 1);
    assert_eq!(
        report.decisions[0].decision,
        Decision::PublishRetained,
        "the reconciled outcome was retained"
    );

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("apply_patch"), 1, "the patch ran once");
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn repeated_crashes_of_a_retried_verification_converge() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "run_verification"))
        .await;
    {
        // The redispatched attempt runs, but its supervisor dies before the receipt.
        let hook = CrashHook::at(CrashPoint::DuringExecute, "run_verification");
        let hooked = ExecOpts {
            exit_before_receipt: Some("run_verification"),
            ..ExecOpts::default()
        };
        let ctl = w.open_with(Some(hook.clone()), &hooked);
        let err = recover_with(
            &ctl.db,
            &ctl.blobs,
            &ctl.exec,
            &w.task,
            &RunOptions::crash_with(hook),
        )
        .await;
        assert!(
            matches!(err, Err(EngineError::Crashed(CrashPoint::DuringExecute))),
            "{err:?}"
        );
    }
    let ctl = w.open(None);
    let report = ctl.recover(&w).await;
    assert_eq!(report.decisions[0].decision, Decision::Redispatch);

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(
        w.counts.get("run_verification"),
        2,
        "the receipt-less attempt and the final one"
    );
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
        deadline_ts: 0,
    }
}

fn retained(ctl: &Ctl, effect: &EffectId) -> ExecOutcome {
    ctl.exec
        .retained_outcome(effect)
        .expect("the executor retained an outcome")
}

fn attempt(lease: u64) -> AttemptCtx {
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: lease,
        worker: "zombie".into(),
    }
}

/// A job request for an attempt of `rec`, as the executor would write it.
fn job_request(
    w: &World,
    ctl: &Ctl,
    rec: &EffectRecord,
    ctx: &AttemptCtx,
    lease_expiry_ms: i64,
) -> JobRequest {
    JobRequest {
        effect_id: rec.effect_id.clone(),
        task_id: w.task.clone(),
        kind: rec.kind.clone(),
        payload: Vec::new(),
        contract: ctl.db.contract(&w.task).unwrap(),
        attempt_id: ctx.attempt_id.clone(),
        lease_generation: ctx.lease_generation,
        lease_expiry_ms,
        task_deadline_ms: 0,
        worker: worker_config(w.dir.path()),
    }
}

/// Leaves `out` as the receipt of a fresh, dead job directory of its effect, the way a job
/// (or a forger) would.
fn leave_receipt(w: &World, ctl: &Ctl, rec: &EffectRecord, out: &ExecOutcome) {
    let ctx = AttemptCtx {
        attempt_id: out.receipt.attempt_id.clone(),
        lease_generation: out.receipt.lease_generation,
        worker: "zombie".into(),
    };
    let (job, lock) = JobDir::create(&w.path("jobs"), &job_request(w, ctl, rec, &ctx, 0)).unwrap();
    drop(lock);
    job.write_receipt(out).unwrap();
}

#[tokio::test]
async fn stale_and_duplicate_receipts_are_ignored_and_audited() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "run_verification"))
        .await;
    {
        // Recovery re-dispatches under lease 2, then dies before executing.
        let hook = CrashHook::new(|p, ctx| {
            p == CrashPoint::AfterDispatch && ctx.kind == Some("run_verification")
        });
        let ctl = w.open(Some(hook.clone()));
        let err = recover_with(
            &ctl.db,
            &ctl.blobs,
            &ctl.exec,
            &w.task,
            &RunOptions::crash_with(hook),
        )
        .await;
        assert!(
            matches!(err, Err(EngineError::Crashed(CrashPoint::AfterDispatch))),
            "{err:?}"
        );
    }
    let ctl = w.open(None);
    let verify = ctl.effect(&w, "run_verification");
    assert_eq!(
        (verify.state, verify.lease_generation),
        (EffectState::Dispatched, 2)
    );

    // A zombie of the first attempt (lease 1) reports directly, and another left a forged
    // receipt in a (dead) job directory of its own.
    let zombie = ExecOutcome::success(
        &request(&ctl, &w, &verify),
        &attempt(1),
        b"{\"passed\": true}".to_vec(),
    );
    let before = (
        ctl.db.task(&w.task).unwrap(),
        ctl.db.usage_summary(&w.task).unwrap(),
    );
    let verdict = ctl
        .db
        .complete_effect(&verify.effect_id, &zombie.receipt, None, None)
        .unwrap();
    assert_eq!(verdict, ReceiptVerdict::StaleLeaseIgnored);
    assert_eq!(
        (
            ctl.db.task(&w.task).unwrap(),
            ctl.db.usage_summary(&w.task).unwrap()
        ),
        before
    );
    let forged_out = ExecOutcome::success(
        &request(&ctl, &w, &verify),
        &attempt(1),
        b"{\"passed\": true}".to_vec(),
    );
    leave_receipt(&w, &ctl, &verify, &forged_out);

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(
        report.decisions[0].decision,
        Decision::Redispatch,
        "the stale receipt is not applied"
    );
    let ignored = ctl.of_type(&w, "ReceiptIgnored");
    assert_eq!(
        ignored.len(),
        2,
        "the direct report and the stale retained one"
    );
    assert!(
        ignored.iter().all(|e| e["reason"] == "StaleLeaseIgnored"),
        "{ignored:?}"
    );
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 3);

    // A duplicate of the applied receipt after the fact changes nothing either.
    let done = ctl.effect(&w, "run_verification");
    let applied = retained(&ctl, &done.effect_id);
    let verdict = ctl
        .db
        .complete_effect(
            &done.effect_id,
            &applied.receipt,
            done.result_digest.as_ref(),
            None,
        )
        .unwrap();
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
    cancel_after_crash_with(point, kind, &ExecOpts::default()).await
}

async fn cancel_after_crash_with(
    point: CrashPoint,
    kind: &'static str,
    opts: &ExecOpts,
) -> (World, Ctl, RecoveryReport) {
    let w = World::new();
    w.crash_run_with(CrashHook::at(point, kind), opts).await;
    let ctl = w.open(None);
    ctl.db.append(&w.task, &TaskEvent::CancelRequested).unwrap();
    let report = ctl.recover(&w).await;
    assert_eq!(report.state, Some(TaskState::Cancelled), "{report:?}");
    assert_eq!(ctl.db.task(&w.task).unwrap().state, TaskState::Cancelled);
    assert_eq!(ctl.of_type(&w, "CancelCompleted").len(), 1);
    // Nothing more happens on a further run.
    let n = ctl.events(&w).len();
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Cancelled
    );
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
            (
                usage.reserved_tool_actions,
                usage.settled_tool_actions,
                usage.uncertain_tool_actions
            ),
            (0, 1, 0),
            "{point}: only the snapshot's action is consumed; the patch's is released"
        );
        assert_eq!(ctl.of_type(&w, "EffectAbandoned").len(), 1);
    }
}

#[tokio::test]
async fn cancel_after_a_crash_completes_a_retained_verification_without_success() {
    let (w, ctl, report) =
        cancel_after_crash(CrashPoint::AfterExecuteBeforePublish, "run_verification").await;
    assert_eq!(report.decisions[0].decision, Decision::PublishRetained);
    assert_eq!(
        ctl.effect(&w, "run_verification").state,
        EffectState::Completed
    );
    let rejected = ctl.of_type(&w, "TaskEventRejected");
    assert_eq!(rejected.len(), 1);
    assert!(
        rejected[0]["event"].get("VerifyPassed").is_some(),
        "{rejected:?}"
    );
    assert_eq!(ctl.db.task(&w.task).unwrap().verified_digest, None);
    assert_eq!(w.counts.get("run_verification"), 1);
}

#[tokio::test]
async fn cancel_after_a_crash_leaves_an_unprovable_verification_unknown() {
    // The verification ran, but its supervisor died before the receipt.
    let hooked = ExecOpts {
        exit_before_receipt: Some("run_verification"),
        ..ExecOpts::default()
    };
    let (w, ctl, report) =
        cancel_after_crash_with(CrashPoint::DuringExecute, "run_verification", &hooked).await;
    assert_eq!(report.decisions[0].decision, Decision::MarkUnknown);
    let verify = ctl.effect(&w, "run_verification");
    assert_eq!(
        verify.state,
        EffectState::Unknown,
        "it ran and left no receipt: it may have happened"
    );
    assert_eq!(ctl.db.outstanding_effects(&w.task).unwrap(), vec![verify]);
    assert_eq!(
        w.counts.get("run_verification"),
        1,
        "not retried once cancel was requested"
    );
    // Recovering again changes nothing.
    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(ctl.events(&w).len(), n);
}

#[tokio::test]
async fn an_unreconcilable_patch_fails_the_task_and_keeps_its_reservation_uncertain() {
    let w = World::new();
    let hooked = ExecOpts {
        exit_before_receipt: Some("apply_patch"),
        ..ExecOpts::default()
    };
    w.crash_run_with(
        CrashHook::at(CrashPoint::DuringExecute, "apply_patch"),
        &hooked,
    )
    .await;
    let ctl = w.open(None);
    // The patch job ends without a receipt ...
    let job = w.only_job(&ctl.effect(&w, "apply_patch").effect_id);
    wait_until("the patch job to end", || job.is_dead()).await;
    assert_eq!(job.read_receipt(), None);
    // ... and someone else touched the workspace too: it is neither the base nor base + patch.
    write_in_workspace(
        w.dir.path(),
        &w.task,
        "src/__init__.py",
        "# changed by hand\n",
    );

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions[0].decision, Decision::Unreconcilable);
    assert_eq!(report.state, Some(TaskState::Failed));
    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(patch.state, EffectState::Unknown);
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0]["Failed"]["reason"],
        format!("unreconcilable effect {}", patch.effect_id)
    );
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!(
        (
            usage.uncertain_tool_actions,
            usage.reserved_tool_actions,
            usage.settled_tool_actions
        ),
        (1, 0, 1)
    );
    assert_eq!(ctl.db.outstanding_effects(&w.task).unwrap(), vec![patch]);

    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Failed
    );
    assert_eq!(ctl.events(&w).len(), n, "a failed task is left alone");
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn a_workspace_lost_with_a_patch_in_flight_fails_the_task_cleanly() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch"))
        .await;
    lose_workspace(w.dir.path(), &w.task);
    let ctl = w.open(None);

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Failed
    );

    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(patch.state, EffectState::Unknown);
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(
        failed[0]["Failed"]["reason"],
        format!("unreconcilable effect {}", patch.effect_id)
    );
    let n = ctl.events(&w).len();
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Failed
    );
    assert_eq!(ctl.events(&w).len(), n);
    assert_eq!(w.counts.get("apply_patch"), 0);
}

#[tokio::test]
async fn a_workspace_lost_between_effects_fails_the_task_cleanly() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterComplete, "apply_patch"))
        .await;
    lose_workspace(w.dir.path(), &w.task);
    let ctl = w.open(None);

    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Failed
    );

    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["Failed"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("workspace lost"),
        "{failed:?}"
    );
    assert_eq!(
        ctl.of_type(&w, "VerifyStarted").len(),
        0,
        "no work on a workspace that is gone"
    );
    let n = ctl.events(&w).len();
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Failed
    );
    assert_eq!(ctl.events(&w).len(), n);
}

#[tokio::test]
async fn a_paused_task_defers_recovery_of_work_it_cannot_dispatch() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterIntent, "apply_patch"))
        .await;
    let ctl = w.open(None);
    ctl.db.append(&w.task, &TaskEvent::Paused).unwrap();

    let report = ctl.recover(&w).await;

    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(report.deferred, vec![patch.effect_id.clone()]);
    assert!(report.decisions.is_empty());
    assert_eq!(patch.state, EffectState::Intended);
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Paused
    );
    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 0);

    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    // A new session after the resume: the agent starts over from the current workspace.
    let mut agent = FakeAgent::scripted(vec![AgentAction::Verify, AgentAction::Finish]);
    let state = run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task)
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
    assert_eq!(w.counts.get("apply_patch"), 1);
    let task = ctl.db.task(&w.task).unwrap();
    assert_eq!(
        task.verified_digest,
        Some(workspace_digest(&w.ws()).unwrap())
    );
}

#[tokio::test]
async fn an_agent_that_replays_differently_fails_the_task() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterComplete, "apply_patch"))
        .await;
    let ctl = w.open(None);
    let mut other = FakeAgent::scripted(vec![AgentAction::ApplyPatch(comment_patch())]);

    let err = run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut other, &w.task)
        .await
        .unwrap_err();

    assert!(
        matches!(err, EngineError::NondeterministicAgent { turn: 1, .. }),
        "{err:?}"
    );
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["Failed"]["reason"]
            .as_str()
            .unwrap()
            .contains("replay"),
        "{failed:?}"
    );
    assert_eq!(
        w.counts.get("apply_patch"),
        1,
        "nothing was executed for the divergent agent"
    );
}

#[tokio::test]
async fn replay_feeds_the_journaled_observations_and_executes_nothing_twice() {
    let w = World::new();
    w.crash_run(CrashHook::at(
        CrashPoint::AfterAgentTurnJournaled,
        "run_verification",
    ))
    .await;
    let ctl = w.open(None);
    let turns = ctl.of_type(&w, "AgentTurn");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["action"], serde_json::json!("Verify"));
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(
        run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task)
            .await
            .unwrap(),
        TaskState::Succeeded
    );

    let journaled: Vec<_> = turns
        .iter()
        .map(|t| serde_json::from_value(t["observation"].clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        agent.observations(),
        &journaled[..],
        "replayed exactly the journaled observations"
    );
    assert_eq!(
        ctl.of_type(&w, "AgentTurn").len(),
        2,
        "replayed turns are not journaled again"
    );
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
        let reserve = Reservation {
            tool_actions: 0,
            model_requests: 1,
        };
        let base = ctl.db.task(&w.task).unwrap().workspace_digest;
        let rec = ctl
            .db
            .record_intent(
                &w.task,
                kind,
                Digest::of(b"prompt"),
                &base,
                reserve,
                &Resource::Task,
            )
            .unwrap();
        ctl.db
            .mark_dispatched(&rec.effect_id, &AttemptId::new(), "model-broker", 1)
            .unwrap();
        rec.effect_id
        // The controller dies with the model call in flight.
    };
    let ctl = w.open(None);

    let report = ctl.recover(&w).await;

    assert_eq!(report.decisions.len(), 1);
    assert_eq!(
        report.decisions[0].decision,
        Decision::Unreconcilable,
        "ExportBundle cannot be retried blindly"
    );
    let rec = ctl.db.effect(&effect).unwrap();
    assert_eq!(rec.state, EffectState::Unknown);
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!(
        (
            usage.uncertain_model_requests,
            usage.reserved_model_requests,
            usage.settled_model_requests
        ),
        (1, 0, 0),
        "neither released nor doubled"
    );
    let journaled = ctl.of_type(&w, "RecoveryDecision");
    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0]["decision"], "Unreconcilable");
    assert_eq!(journaled[0]["found_state"], "Dispatched");
    assert!(
        journaled[0]["reason"]
            .as_str()
            .unwrap()
            .contains("ReconcileThenRetry"),
        "{journaled:?}"
    );
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
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    let verify = ctl.effect(&w, "run_verification");
    let real = retained(&ctl, &verify.effect_id);
    assert_eq!(real.receipt.lease_generation, 1);
    assert_eq!(Some(Digest::of(&real.output)), verify.result_digest);
    assert_eq!(real.receipt.outcome, Outcome::Success);

    let newer = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(7), b"newer".to_vec());
    let older = ExecOutcome::success(&request(&ctl, &w, &verify), &attempt(0), b"older".to_vec());
    for out in [&newer, &older] {
        leave_receipt(&w, &ctl, &verify, out);
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
async fn interrupted_run(
    point: CrashPoint,
    kind: &'static str,
    event: TaskEvent,
) -> (World, Ctl, TaskState) {
    let w = World::new();
    let ctl = w.open(None);
    let opts = interrupt_at(&w, point, kind, event);
    let state = w
        .run(&ctl, &opts)
        .await
        .unwrap_or_else(|e| panic!("{point} {kind}: the run must end cleanly, got {e:?}"));
    assert_journal_sound(&w, &ctl);
    (w, ctl, state)
}

async fn resume_with(w: &World, ctl: &Ctl, actions: Vec<AgentAction>) -> TaskState {
    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    let mut agent = FakeAgent::scripted(actions);
    run_task(&ctl.db, &ctl.blobs, &ctl.exec, &mut agent, &w.task)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_pause_landing_after_the_intent_stops_the_run_and_resume_completes_it() {
    let (w, ctl, state) =
        interrupted_run(CrashPoint::AfterIntent, "apply_patch", TaskEvent::Paused).await;
    assert_eq!(state, TaskState::Paused);
    assert_eq!(
        ctl.effect(&w, "apply_patch").state,
        EffectState::Intended,
        "not dispatched while paused"
    );
    assert_eq!(w.counts.get("apply_patch"), 0);

    // Resume: recovery dispatches the intended patch, a new session verifies it.
    assert_eq!(
        resume_with(&w, &ctl, vec![AgentAction::Verify, AgentAction::Finish]).await,
        TaskState::Succeeded
    );
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
}

#[tokio::test]
async fn a_cancel_landing_after_the_intent_cancels_and_releases_the_intent() {
    let (w, ctl, state) = interrupted_run(
        CrashPoint::AfterIntent,
        "apply_patch",
        TaskEvent::CancelRequested,
    )
    .await;
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Abandoned);
    assert!(ctl.db.outstanding_effects(&w.task).unwrap().is_empty());
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!(
        (usage.reserved_tool_actions, usage.settled_tool_actions),
        (0, 1)
    );
    assert_eq!(workspace_digest(&w.ws()).unwrap(), w.base());
    assert!(ctl.recover(&w).await.decisions.is_empty());
}

#[tokio::test]
async fn a_pause_landing_after_a_journaled_patch_turn_stops_before_the_intent() {
    let baseline = baseline().await;
    let (w, ctl, state) = interrupted_run(
        CrashPoint::AfterAgentTurnJournaled,
        "apply_patch",
        TaskEvent::Paused,
    )
    .await;
    assert_eq!(state, TaskState::Paused);
    assert!(
        ctl.effects(&w)
            .iter()
            .all(|e| e.kind.tag() == "read_snapshot"),
        "no patch effect"
    );

    // The resumed session starts over from Start and runs the whole solution once.
    ctl.db.append(&w.task, &TaskEvent::Resumed).unwrap();
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

#[tokio::test]
async fn a_cancel_landing_after_a_journaled_verify_turn_cancels_without_verifying() {
    let (w, ctl, state) = interrupted_run(
        CrashPoint::AfterAgentTurnJournaled,
        "run_verification",
        TaskEvent::CancelRequested,
    )
    .await;
    assert_eq!(state, TaskState::Cancelled);
    assert_eq!(ctl.of_type(&w, "VerifyStarted").len(), 0);
    assert!(
        ctl.effects(&w)
            .iter()
            .all(|e| e.kind.tag() != "run_verification")
    );
    assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Completed);
}

#[tokio::test]
async fn a_pause_landing_after_a_journaled_verify_turn_pauses_and_resume_verifies() {
    let (w, ctl, state) = interrupted_run(
        CrashPoint::AfterAgentTurnJournaled,
        "run_verification",
        TaskEvent::Paused,
    )
    .await;
    assert_eq!(state, TaskState::Paused);
    assert_eq!(ctl.of_type(&w, "VerifyStarted").len(), 0);

    assert_eq!(
        resume_with(&w, &ctl, vec![AgentAction::Verify]).await,
        TaskState::Succeeded
    );
    let task = ctl.db.task(&w.task).unwrap();
    assert_eq!(
        task.verified_digest,
        Some(workspace_digest(&w.ws()).unwrap())
    );
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
}

/// The controller dies right after launching a verification that takes a while: recovery
/// waits for the job, publishes its receipt and never starts a second attempt.
#[tokio::test]
async fn recovery_waits_for_a_running_job_and_publishes_its_receipt_without_a_second_attempt() {
    let baseline = baseline().await;
    let w = World::new();
    w.slow_profile(2.5);
    assert_eq!(
        w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "run_verification"))
            .await,
        CrashPoint::DuringExecute
    );
    let ctl = w.open(None);
    let verify = ctl.effect(&w, "run_verification");
    let job = w.only_job(&verify.effect_id);
    assert!(!job.is_dead(), "the controller died with the job running");

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::PublishRetained], "{report:?}");
    assert_eq!(ctl.of_type(&w, "RecoveryDecision").len(), 1);
    // The audit records that the receipt came from waiting for a live job.
    assert!(report.decisions[0].reason.contains("waiting"), "{report:?}");
    assert_eq!(w.jobs(&verify.effect_id).len(), 1, "no second attempt");
    let verify = ctl.effect(&w, "run_verification");
    assert_eq!(
        (verify.state, verify.lease_generation),
        (EffectState::Completed, 1)
    );
    assert_eq!(
        verify.result_digest,
        Some(Digest::of(&job.read_receipt().unwrap().output))
    );
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(ctl.db.usage_summary(&w.task).unwrap(), baseline.usage);
    let task = ctl.db.task(&w.task).unwrap();
    assert_eq!(
        task.verified_digest,
        Some(workspace_digest(&w.ws()).unwrap())
    );
    assert_eq!(w.counts(), KINDS.iter().map(|k| (*k, 1)).collect());
    assert_journal_sound(&w, &ctl);
    w.assert_no_live_process().await;
}

/// The supervisor of a running verification is SIGKILLed while the controller is down: its
/// lock is free, so the job is dead, but its worker and check live on. Recovery kills them
/// before it runs the verification again, so the two attempts never overlap.
#[tokio::test]
async fn recovery_after_a_supervisor_sigkill_redispatches_a_verification_after_fencing_the_orphan_worker()
 {
    let baseline = baseline().await;
    let w = World::new();
    w.slow_profile(30.0);
    w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "run_verification"))
        .await;
    let ctl = w.open(None);
    let verify = ctl.effect(&w, "run_verification");
    let first = w.only_job(&verify.effect_id);
    let supervisor = running_supervisor(&first).await;
    kill_process(Pid::from_raw(supervisor).unwrap(), Signal::KILL).unwrap();
    wait_until("the first job's lock to be free", || first.is_dead()).await;
    assert!(
        !session_members(supervisor).is_empty(),
        "the orphaned worker outlived its supervisor"
    );
    w.restore_profile();

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::Redispatch], "{report:?}");
    let jobs = w.jobs(&verify.effect_id);
    assert_eq!(jobs.len(), 2);
    assert!(jobs[0].is_dead() && jobs[0].read_receipt().is_none());
    assert_eq!(
        session_members(supervisor),
        Vec::<i32>::new(),
        "a process of the first attempt lives"
    );
    assert_eq!(processes_naming(&first.path), Vec::<i32>::new());
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 2);
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("run_verification"), 2);
    assert_journal_sound(&w, &ctl);
    w.assert_no_live_process().await;
}

/// A stopped supervisor keeps its lock past its lease: recovery waits out the lease plus the
/// grace, fences the job (the cancel marker goes unheard, so the controller kills it), and
/// only then runs the verification again.
#[tokio::test]
async fn recovery_with_a_sigstopped_supervisor_fences_then_redispatches() {
    let baseline = baseline().await;
    let w = World::new();
    w.slow_profile(30.0);
    let short = ExecOpts {
        timeouts: Some(EffectTimeouts {
            verification: Duration::from_secs(2),
            other: Duration::from_secs(30),
        }),
        ..ExecOpts::default()
    };
    w.crash_run_with(
        CrashHook::at(CrashPoint::DuringExecute, "run_verification"),
        &short,
    )
    .await;
    let ctl = w.open(None);
    let verify = ctl.effect(&w, "run_verification");
    let first = w.only_job(&verify.effect_id);
    let supervisor = running_supervisor(&first).await;
    let _stopped = Stopped::stop(supervisor);
    w.restore_profile();

    let started = Instant::now();
    let report = ctl.recover(&w).await;

    let lease_expiry = first.request().unwrap().lease_expiry_ms;
    assert!(
        now_ms() >= lease_expiry + 5_000,
        "fenced only past the lease plus the grace"
    );
    assert!(started.elapsed() < PATIENCE);
    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        vec![Decision::WaitedForJob, Decision::Redispatch],
        "{report:?}"
    );
    let journaled = ctl.of_type(&w, "RecoveryDecision");
    assert_eq!(journaled.len(), 2);
    assert_eq!(journaled[0]["decision"], "WaitedForJob");
    let jobs = w.jobs(&verify.effect_id);
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs[0].is_dead() && jobs[0].read_receipt().is_none(),
        "killed mid-flight: no receipt"
    );
    assert!(jobs[0].cancel_requested());
    assert_eq!(
        session_members(supervisor),
        Vec::<i32>::new(),
        "a process of the first attempt lives"
    );
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 2);
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("run_verification"), 2);
    assert_journal_sound(&w, &ctl);
    w.assert_no_live_process().await;
}

/// A job that cannot be stopped: the effect may still be running, so it is left UNKNOWN,
/// its reservation stays uncertain, and the task fails.
#[tokio::test]
async fn recovery_marks_unknown_when_fencing_cannot_free_the_lock() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch"))
        .await;
    let ctl = w.open_with(
        None,
        &ExecOpts {
            fence: Fence::Fails,
            ..ExecOpts::default()
        },
    );
    let patch = ctl.effect(&w, "apply_patch");
    // A job of the dispatched attempt, past its lease, whose lock this test holds.
    let ctx = AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: patch.lease_generation,
        worker: "w".into(),
    };
    let (_job, _lock) = JobDir::create(
        &w.path("jobs"),
        &job_request(&w, &ctl, &patch, &ctx, now_ms() - 60_000),
    )
    .unwrap();

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        vec![Decision::WaitedForJob, Decision::FenceFailed],
        "{report:?}"
    );
    assert_eq!(report.state, Some(TaskState::Failed));
    let journaled = ctl.of_type(&w, "RecoveryDecision");
    assert_eq!(journaled.len(), 2);
    assert_eq!(
        journaled[0]["decision"], "WaitedForJob",
        "journaled before the fence"
    );
    assert_eq!(journaled[1]["decision"], "FenceFailed");
    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(patch.state, EffectState::Unknown);
    let failed = ctl.of_type(&w, "Failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0]["Failed"]["reason"],
        format!("unreconcilable effect {}", patch.effect_id)
    );
    let usage = ctl.db.usage_summary(&w.task).unwrap();
    assert_eq!(
        (
            usage.uncertain_tool_actions,
            usage.reserved_tool_actions,
            usage.settled_tool_actions
        ),
        (1, 0, 1)
    );
    assert_eq!(ctl.db.outstanding_effects(&w.task).unwrap(), vec![patch]);
    assert_eq!(
        workspace_digest(&w.ws()).unwrap(),
        w.base(),
        "nothing was reconciled or run"
    );

    let n = ctl.events(&w).len();
    assert!(ctl.recover(&w).await.decisions.is_empty());
    assert_eq!(ctl.events(&w).len(), n);
    assert_eq!(w.counts.get("apply_patch"), 0);
    assert_journal_sound(&w, &ctl);
}

/// Every way a controller can die with a job in flight leaves no process of that world
/// behind once recovery is done.
#[tokio::test]
async fn no_live_process_remains_after_any_recovery_case() {
    for kind in KINDS {
        for exit_before_receipt in [false, true] {
            let w = World::new();
            let opts = ExecOpts {
                exit_before_receipt: exit_before_receipt.then_some(kind),
                ..ExecOpts::default()
            };
            w.crash_run_with(CrashHook::at(CrashPoint::DuringExecute, kind), &opts)
                .await;
            let ctl = w.open(None);
            ctl.recover(&w).await;
            w.assert_no_live_process().await;
            assert_job_sessions_empty(&w);
            assert_eq!(
                w.run(&ctl, &RunOptions::default()).await.unwrap(),
                TaskState::Succeeded,
                "{kind}"
            );
            w.assert_no_live_process().await;
            assert_job_sessions_empty(&w);
        }
    }
    // An orphaned check whose supervisor was killed.
    let w = World::new();
    w.slow_profile(30.0);
    w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "run_verification"))
        .await;
    let ctl = w.open(None);
    let job = w.only_job(&ctl.effect(&w, "run_verification").effect_id);
    let supervisor = running_supervisor(&job).await;
    kill_process(Pid::from_raw(supervisor).unwrap(), Signal::KILL).unwrap();
    wait_until("the lock to be free", || job.is_dead()).await;
    assert!(!w.live_processes().is_empty());
    w.restore_profile();
    ctl.recover(&w).await;
    w.assert_no_live_process().await;
    assert_job_sessions_empty(&w);
}

/// One attempt's `[start, last sign of life]` in nanoseconds, from the files the script left.
fn intervals(dir: &Path) -> Vec<(u128, u128)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "start") {
            let start: u128 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();
            let beats = fs::read_to_string(path.with_extension("beats")).unwrap_or_default();
            let last = beats
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .max()
                .unwrap_or(start);
            found.push((start, last));
        }
    }
    found.sort();
    found
}

#[tokio::test]
async fn two_attempts_of_the_same_effect_never_run_concurrently() {
    let w = World::new();
    let beats = w.path("beats");
    fs::create_dir(&beats).unwrap();
    // Each attempt records its start and then a heartbeat every ~25 ms for about a second.
    let script = format!(
        "f={}/$$; date +%s%N > $f.tmp && mv $f.tmp $f.start; i=0; while [ $i -lt 40 ]; do date +%s%N >> $f.beats; sleep 0.025; i=$((i+1)); done; echo checked",
        beats.display()
    );
    let opts = ExecOpts {
        scripted_verification: Some(script),
        ..ExecOpts::default()
    };
    w.crash_run_with(
        CrashHook::at(CrashPoint::DuringExecute, "run_verification"),
        &opts,
    )
    .await;
    let ctl = w.open_with(None, &opts);
    let verify = ctl.effect(&w, "run_verification");
    let first = w.only_job(&verify.effect_id);
    let supervisor = running_supervisor(&first).await;
    wait_until("the first heartbeat", || {
        intervals(&beats).first().is_some_and(|(s, l)| l > s)
    })
    .await;
    kill_process(Pid::from_raw(supervisor).unwrap(), Signal::KILL).unwrap();
    wait_until("the first job's lock to be free", || first.is_dead()).await;

    let report = ctl.recover(&w).await;

    assert_eq!(
        report
            .decisions
            .iter()
            .map(|d| d.decision)
            .collect::<Vec<_>>(),
        vec![Decision::Redispatch]
    );
    assert_eq!(
        ctl.effect(&w, "run_verification").state,
        EffectState::Completed
    );
    // Let a surviving first attempt (if any) show itself.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let found = intervals(&beats);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found[0].1 < found[1].0,
        "the attempts overlapped: {found:?}"
    );
    assert_eq!(w.counts.get("run_verification"), 2);
    w.assert_no_live_process().await;
}

/// A job launched under long timeouts is still inside its lease when a controller
/// configured with much shorter ones recovers: it is waited for (its own lease counts, not
/// the recovering controller's timeouts), never fenced.
#[tokio::test]
async fn recovery_never_fences_a_job_inside_its_own_lease_under_shorter_timeouts() {
    let w = World::new();
    // Longer than the recovering controller's own longest timeout plus the 5 s grace.
    w.slow_profile(6.5);
    w.crash_run(CrashHook::at(CrashPoint::DuringExecute, "run_verification"))
        .await;
    let short = EffectTimeouts {
        verification: Duration::from_millis(100),
        other: Duration::from_millis(100),
    };
    let ctl = w.open_with(
        None,
        &ExecOpts {
            timeouts: Some(short),
            ..ExecOpts::default()
        },
    );
    let verify = ctl.effect(&w, "run_verification");
    let job = w.only_job(&verify.effect_id);
    assert!(
        job.request().unwrap().lease_expiry_ms > now_ms() + 60_000,
        "launched under the 70 s lease"
    );

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::PublishRetained], "{report:?}");
    assert!(!job.cancel_requested(), "the job was not fenced");
    assert_eq!(w.jobs(&verify.effect_id).len(), 1);
    assert_eq!(ctl.effect(&w, "run_verification").lease_generation, 1);
    assert_eq!(w.counts.get("run_verification"), 1);
    w.assert_no_live_process().await;
}

/// The controller dies while fencing: the fence decision is already journaled (decisions
/// come before the action), and the next recovery still converges.
#[tokio::test]
async fn a_crash_during_the_fence_still_converges() {
    let baseline = baseline().await;
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "apply_patch"))
        .await;
    let lock = {
        let ctl = w.open_with(
            None,
            &ExecOpts {
                fence: Fence::Hangs,
                ..ExecOpts::default()
            },
        );
        let patch = ctl.effect(&w, "apply_patch");
        // A job of the dispatched attempt, past its lease, whose lock this test holds.
        let ctx = AttemptCtx {
            attempt_id: AttemptId::new(),
            lease_generation: patch.lease_generation,
            worker: "w".into(),
        };
        let (_job, lock) = JobDir::create(
            &w.path("jobs"),
            &job_request(&w, &ctl, &patch, &ctx, now_ms() - 60_000),
        )
        .unwrap();
        let died = tokio::time::timeout(Duration::from_secs(2), ctl.recover(&w)).await;
        assert!(died.is_err(), "the fence never returned");
        let journaled = ctl.of_type(&w, "RecoveryDecision");
        assert_eq!(journaled.len(), 1);
        assert_eq!(
            journaled[0]["decision"], "WaitedForJob",
            "journaled before the fence ran"
        );
        assert_eq!(ctl.effect(&w, "apply_patch").state, EffectState::Dispatched);
        lock
    };
    // The job dies meanwhile (its lock is released).
    drop(lock);
    let ctl = w.open(None);

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::Redispatch], "{report:?}");
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
}

/// A jobs root that cannot be listed says nothing about the jobs in it: recovery must not
/// conclude "no job" and run the effect again beside one.
#[tokio::test]
async fn an_unreadable_jobs_root_never_leads_to_a_redispatch() {
    let w = World::new();
    w.crash_run(CrashHook::at(CrashPoint::AfterDispatch, "run_verification"))
        .await;
    let ctl = w.open(None);
    fs::remove_dir_all(w.path("jobs")).unwrap();
    fs::write(w.path("jobs"), b"").unwrap();

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        vec![Decision::WaitedForJob, Decision::FenceFailed],
        "{report:?}"
    );
    assert_eq!(
        ctl.effect(&w, "run_verification").state,
        EffectState::Unknown
    );
    assert_eq!(ctl.db.task(&w.task).unwrap().state, TaskState::Failed);
    assert_eq!(w.counts.get("run_verification"), 0);
}

/// A patch job whose supervisor stopped after the patch was applied and before any receipt
/// is fenced past its lease; the patch is then reconciled (applied once), never recorded as
/// failed.
#[tokio::test]
async fn a_fenced_patch_job_that_applied_its_patch_is_reconciled_not_failed() {
    let baseline = baseline().await;
    let w = World::new();
    let opts = ExecOpts {
        slow_poll: Some("apply_patch"),
        timeouts: Some(EffectTimeouts {
            verification: Duration::from_secs(70),
            other: Duration::from_secs(2),
        }),
        ..ExecOpts::default()
    };
    w.crash_run_with(
        CrashHook::at(CrashPoint::DuringExecute, "apply_patch"),
        &opts,
    )
    .await;
    let ctl = w.open(None);
    let patch = ctl.effect(&w, "apply_patch");
    let job = w.only_job(&patch.effect_id);
    // The supervisor sleeps between polls; the worker finishes the patch meanwhile.
    wait_until("the worker's outcome", || job.read_outcome().is_some()).await;
    let supervisor = job.read_status().unwrap().supervisor_pid.unwrap() as i32;
    let _stopped = Stopped::stop(supervisor);
    assert_eq!(job.read_receipt(), None);
    assert_ne!(
        workspace_digest(&w.ws()).unwrap(),
        w.base(),
        "the patch is applied"
    );

    let report = ctl.recover(&w).await;

    let decisions: Vec<_> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(
        decisions,
        vec![Decision::WaitedForJob, Decision::PublishReconciled],
        "{report:?}"
    );
    assert_eq!(ctl.of_type(&w, "EffectFailed").len(), 0);
    let patch = ctl.effect(&w, "apply_patch");
    assert_eq!(
        (patch.state, patch.lease_generation),
        (EffectState::Completed, 1)
    );
    assert!(job.is_dead() && job.read_receipt().is_none());
    assert_eq!(
        w.run(&ctl, &RunOptions::default()).await.unwrap(),
        TaskState::Succeeded
    );
    assert_eq!(summarize(&w, &ctl), baseline);
    assert_eq!(w.counts.get("apply_patch"), 1);
    assert_journal_sound(&w, &ctl);
    w.assert_no_live_process().await;
    assert_job_sessions_empty(&w);
}
