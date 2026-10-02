//! The controller side of supervised execution: every effect attempt runs as a job (see
//! [`crate::job`]) owned by its own `agentos-supervisor` process, so the controller can die
//! and come back without losing the effect or its receipt.
//!
//! [`SupervisedExecutor::run`] creates the job directory (which takes the job's lock),
//! launches the supervisor with that locked file as its stdin, and waits for the receipt.
//! Liveness is only ever [`JobDir::is_dead`]: the lock, never a pid. A job that dies without
//! a receipt follows the kill-receipt rule: a retryable kind is a failure; a patch is
//! reconciled, and if reconciliation cannot tell, the outcome is `unresolved` (the runner
//! then marks the effect UNKNOWN and fails the task). The wait is bounded by the job's lease
//! plus a grace period; past it the job is fenced, and a job that survives the fence is
//! `unresolved` too: no outcome is ever invented for a job that may still be running.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::{EffectId, EffectKind, RetryPolicy};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::lease::{lease_expiry_ms, EffectTimeouts};
use rustix::process::{getpgrp, getpid, getsid, kill_process, kill_process_group, Pid, Signal};

use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use crate::fixture::FixtureExecutor;
use crate::job::{JobDir, JobRequest, WorkerConfig};
use crate::supervisor::SupervisorCmd;

/// How often a job directory is looked at while waiting.
const POLL: Duration = Duration::from_millis(25);
/// How long past its lease a job may take to die on its own before it is fenced.
const GRACE_MS: i64 = 5_000;
/// How long a fenced job gets to stop cooperatively (on the cancel marker) before the
/// controller kills its processes.
const FENCE_GRACE_MS: i64 = 2_000;
/// How long the lock may take to come free after the kills.
const KILL_SETTLE_MS: i64 = 2_000;

/// How many times each effect kind was really executed, shared across executor instances
/// so it survives a simulated restart.
#[derive(Debug, Clone, Default)]
pub struct ExecCounts(Arc<[AtomicUsize; 4]>);

fn slot(tag: &str) -> usize {
    match tag {
        "read_snapshot" => 0,
        "apply_patch" => 1,
        "run_verification" => 2,
        _ => 3,
    }
}

impl ExecCounts {
    pub(crate) fn record(&self, kind: &EffectKind) {
        self.0[slot(kind.tag())].fetch_add(1, Ordering::SeqCst);
    }

    /// Executions of the kind with tag `tag` (`EffectKind::tag`).
    pub fn get(&self, tag: &str) -> usize {
        self.0[slot(tag)].load(Ordering::SeqCst)
    }
}

/// What waiting for an effect's jobs found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobWait {
    /// Every attempt is dead; this is the receipt of the highest lease generation.
    Receipt(Box<ExecOutcome>),
    /// Every attempt is dead and none left a receipt (or there is no attempt at all).
    Dead,
    /// Some attempt still holds its lock after the bound.
    StillAlive,
}

pub struct SupervisedExecutor {
    jobs_root: PathBuf,
    supervisor: SupervisorCmd,
    worker: WorkerConfig,
    timeouts: EffectTimeouts,
    counts: ExecCounts,
    crash: Option<CrashHook>,
    extra_env: Vec<(String, String)>,
    /// Answers `reconcile` and `current_workspace` for host workers, read-only. The 3a
    /// stand-in: the workspace lives on the host, so the controller can inspect it directly;
    /// 3b moves it into the guest and this into the worker.
    reconciler: Option<FixtureExecutor>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Waits until every job in `jobs` is dead or the clock reaches `until_ms`; returns whether
/// they all died.
async fn wait_dead(jobs: &[JobDir], until_ms: i64) -> bool {
    loop {
        if jobs.iter().all(JobDir::is_dead) {
            return true;
        }
        if now_ms() >= until_ms {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The receipt in `job`, if it is for this very attempt.
fn receipt_for(job: &JobDir, req: &EffectRequest, ctx: &AttemptCtx) -> Option<ExecOutcome> {
    let out = job.read_receipt()?;
    let r = &out.receipt;
    if r.effect_id == req.effect_id && r.attempt_id == ctx.attempt_id && r.lease_generation == ctx.lease_generation {
        Some(out)
    } else {
        tracing::warn!(job = %job.path.display(), receipt = ?r, "job receipt is for another attempt");
        None
    }
}

/// `id` as a pid or group id we may signal: never init's (or "everyone"), never our own.
fn signalable(id: i32) -> Option<Pid> {
    let me = getpid().as_raw_nonzero().get();
    let my_group = getpgrp().as_raw_nonzero().get();
    if id <= 1 || id == me || id == my_group {
        return None;
    }
    Pid::from_raw(id)
}

/// The recorded supervisor pid, if it still names a supervisor: `status.json` is file
/// content, so the pid must lead its own session (a real supervisor calls `setsid` and
/// refuses to run otherwise) and that session must not be ours.
fn trusted_supervisor(pid: u32) -> Option<Pid> {
    let p = signalable(i32::try_from(pid).ok()?)?;
    let own_session = getsid(None).ok()?;
    (getsid(Some(p)).ok()? == p && p != own_session).then_some(p)
}

/// SIGKILLs what `job` recorded: its worker groups and `groups` entries that live in the
/// supervisor's session (`groups` is worker-writable; a forged entry must not reach any
/// other process), then the supervisor itself, which a stopped supervisor cannot do for us.
fn kill_job(job: &JobDir) {
    let Some(status) = job.read_status() else {
        tracing::warn!(job = %job.path.display(), "no status: no recorded process to kill");
        return;
    };
    let Some(supervisor) = status.supervisor_pid.and_then(trusted_supervisor) else {
        tracing::warn!(job = %job.path.display(), pid = ?status.supervisor_pid, "recorded supervisor pid is not a supervisor");
        return;
    };
    let groups = status.worker_pgid.into_iter().chain(job.groups());
    for pgid in groups {
        let Some(group) = signalable(pgid) else { continue };
        match getsid(Some(group)) {
            Ok(sid) if sid == supervisor => {
                let _ = kill_process_group(group, Signal::KILL);
            }
            Ok(_) => tracing::warn!(job = %job.path.display(), pgid, "not killing a group outside the job's session"),
            Err(_) => {} // the group's leader is gone
        }
    }
    tracing::warn!(job = %job.path.display(), pid = supervisor.as_raw_nonzero().get(), "killing the supervisor");
    let _ = kill_process(supervisor, Signal::KILL);
}

impl SupervisedExecutor {
    pub fn new(
        jobs_root: PathBuf,
        supervisor: SupervisorCmd,
        worker: WorkerConfig,
        counts: ExecCounts,
    ) -> io::Result<SupervisedExecutor> {
        fs::create_dir_all(&jobs_root)?;
        let reconciler = match &worker {
            WorkerConfig::Host(h) => Some(
                FixtureExecutor::new(h.snapshot_dir.clone(), h.profile_dir.clone(), h.work_root.clone())
                    .with_verify_timeout(Duration::from_secs(h.verify_timeout_secs))
                    .with_pinned_profile(h.profile_digest),
            ),
            WorkerConfig::Scripted(_) => None,
        };
        Ok(SupervisedExecutor {
            jobs_root,
            supervisor,
            worker,
            timeouts: EffectTimeouts::default(),
            counts,
            crash: None,
            extra_env: Vec::new(),
            reconciler,
        })
    }

    pub fn with_timeouts(mut self, timeouts: EffectTimeouts) -> SupervisedExecutor {
        self.timeouts = timeouts;
        self
    }

    /// Consults `hook` at `CrashPoint::DuringExecute`, right after the launch: when it fires,
    /// `run` returns at once with a throwaway outcome, as if the controller died with the job
    /// running. Pass a clone of the hook the run uses, so the runner sees the crash.
    pub fn with_crash(mut self, hook: Option<CrashHook>) -> SupervisedExecutor {
        self.crash = hook;
        self
    }

    /// Sets `key` in the supervisor's (and so the worker's) environment.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> SupervisedExecutor {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    pub fn counts(&self) -> &ExecCounts {
        &self.counts
    }

    fn timeout(&self, kind: &EffectKind) -> Duration {
        match kind {
            EffectKind::RunVerification => self.timeouts.verification,
            _ => self.timeouts.other,
        }
    }

    /// Starts `<program> <prefix_args…> run <job_dir>` with `lock` as its stdin. A plain
    /// spawn: the supervisor makes itself a session leader. Our copy of the locked file is
    /// closed as soon as the spawn returns, so the lock lives exactly as long as the
    /// supervisor. The supervisor is reaped by a detached thread so it leaves no zombie;
    /// the handle is never used to judge liveness.
    fn launch(&self, job: &JobDir, lock: fs::File) -> io::Result<()> {
        let spawned = {
            let mut cmd = Command::new(&self.supervisor.program);
            cmd.args(&self.supervisor.prefix_args)
                .arg("run")
                .arg(&job.path)
                .stdin(Stdio::from(lock))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (k, v) in &self.extra_env {
                cmd.env(k, v);
            }
            cmd.spawn()
            // `cmd`, and with it the controller's copy of the lock, is dropped here.
        };
        let mut child = spawned?;
        let reaper = std::thread::Builder::new().name("supervisor-reaper".into()).spawn(move || {
            let _ = child.wait();
        });
        if let Err(e) = reaper {
            tracing::warn!(job = %job.path.display(), error = %e, "cannot reap the supervisor; it stays a zombie");
        }
        Ok(())
    }

    /// The outcome of a job that is dead without a receipt (the kill-receipt rule).
    async fn without_receipt(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind.retry_policy() {
            RetryPolicy::Retry => ExecOutcome::failure(req, ctx, "supervisor died without a receipt"),
            RetryPolicy::ReconcileThenRetry => match self.reconcile(req, ctx).await {
                Reconciliation::Applied(out) => out,
                Reconciliation::NotApplied => ExecOutcome::failure(req, ctx, "patch provably not applied"),
                Reconciliation::Unknown => ExecOutcome::unresolved(
                    req,
                    ctx,
                    "the job died without a receipt and reconciliation cannot tell whether it took effect",
                ),
            },
            RetryPolicy::NoRetry => {
                ExecOutcome::unresolved(req, ctx, "the job died without a receipt and its kind is never retried")
            }
        }
    }

    /// Fences every job in `jobs` that is not dead: drops `cancel`, gives the job
    /// `FENCE_GRACE_MS` to stop, then kills its recorded processes. Returns whether every
    /// job is dead afterwards.
    pub async fn fence_jobs(&self, jobs: &[JobDir]) -> bool {
        let live: Vec<JobDir> = jobs.iter().filter(|j| !j.is_dead()).cloned().collect();
        if live.is_empty() {
            return true;
        }
        for job in &live {
            if let Err(e) = job.drop_cancel() {
                tracing::warn!(job = %job.path.display(), error = %e, "cannot drop the cancel marker");
            }
        }
        if wait_dead(&live, now_ms().saturating_add(FENCE_GRACE_MS)).await {
            return true;
        }
        for job in live.iter().filter(|j| !j.is_dead()) {
            kill_job(job);
        }
        let dead = wait_dead(&live, now_ms().saturating_add(KILL_SETTLE_MS)).await;
        if !dead {
            tracing::warn!(jobs = ?live.iter().map(|j| j.path.display().to_string()).collect::<Vec<_>>(), "jobs survived the fence");
        }
        dead
    }

    /// Waits for every attempt of `effect` to die, until the latest lease among the live
    /// ones plus `GRACE_MS`, but no longer than `bound`. A job whose request cannot be read
    /// has no known lease: only `bound` limits the wait for it.
    pub async fn wait_for_job(&self, effect: &EffectId, bound: Duration) -> JobWait {
        let jobs = JobDir::list(&self.jobs_root, effect);
        let now = now_ms();
        let lease_bound = jobs
            .iter()
            .filter(|j| !j.is_dead())
            .map(|j| j.request().map_or(i64::MAX, |r| r.lease_expiry_ms.saturating_add(GRACE_MS)))
            .max()
            .unwrap_or(now);
        if !wait_dead(&jobs, now.saturating_add(millis(bound)).min(lease_bound)).await {
            return JobWait::StillAlive;
        }
        match self.retained_outcome(effect) {
            Some(out) => JobWait::Receipt(Box::new(out)),
            None => JobWait::Dead,
        }
    }
}

impl Executor for SupervisedExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        self.counts.record(&req.kind);
        let now = now_ms();
        let task_deadline_ms = req.deadline_ts.saturating_mul(1000);
        let lease = lease_expiry_ms(now, millis(self.timeout(&req.kind)), task_deadline_ms);
        if lease <= now {
            return ExecOutcome::failure(req, ctx, "deadline exceeded");
        }
        let job_req = JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms: lease,
            task_deadline_ms,
            worker: self.worker.clone(),
        };
        let (job, lock) = match JobDir::create(&self.jobs_root, &job_req) {
            Ok(created) => created,
            Err(e) => return ExecOutcome::failure(req, ctx, format!("supervisor launch failed: cannot create the job: {e}")),
        };
        if let Err(e) = self.launch(&job, lock) {
            // Nothing runs and the lock is free again: leave no dead job without a receipt.
            if let Err(e) = fs::remove_dir_all(&job.path) {
                tracing::warn!(job = %job.path.display(), error = %e, "cannot remove the unlaunched job");
            }
            return ExecOutcome::failure(req, ctx, format!("supervisor launch failed: {e}"));
        }
        if self.crash.as_ref().is_some_and(|h| h.check(CrashPoint::DuringExecute, Some(req.kind.tag()))) {
            // The controller "dies" with the job running; the runner discards this outcome.
            return ExecOutcome::failure(req, ctx, "injected crash after the launch");
        }
        let jobs = std::slice::from_ref(&job);
        if !wait_dead(jobs, lease.saturating_add(GRACE_MS)).await && !self.fence_jobs(jobs).await {
            return ExecOutcome::unresolved(req, ctx, "the job outlived its lease and could not be stopped");
        }
        match receipt_for(&job, req, ctx) {
            Some(out) => out,
            None => self.without_receipt(req, ctx).await,
        }
    }

    /// The receipt of the highest lease generation among `effect`'s jobs. A receipt flagged
    /// `unresolved` is no receipt.
    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        JobDir::list(&self.jobs_root, effect)
            .iter()
            .filter_map(JobDir::read_receipt)
            .filter(|out| out.receipt.effect_id == *effect && !out.unresolved)
            .max_by_key(|out| out.receipt.lease_generation)
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        match &self.reconciler {
            Some(r) => r.reconcile(req, ctx).await,
            None => Reconciliation::Unknown,
        }
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.reconciler.as_ref().and_then(|r| r.current_workspace(task))
    }

    async fn fence_job(&self, effect: &EffectId) -> bool {
        self.fence_jobs(&JobDir::list(&self.jobs_root, effect)).await
    }
}
