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
//!
//! A job whose supervisor was killed (SIGKILL, OOM) is dead by the lock, but its worker and
//! checks can live on in the dead supervisor's session. Such leftovers are killed before the
//! job counts as settled ([`settled`]), so no new attempt ever runs beside them.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::{EffectId, EffectKind, RetryPolicy};
use agentos_core::guest::{PatchStateKind, mint_attempt_token};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::lease::{EffectTimeouts, MAX_SESSION_TIMEOUT_MS, lease_expiry_ms};
use rustix::process::{
    Pid, PidfdFlags, Signal, getpgrp, getpid, getsid, kill_process_group, pidfd_open,
    pidfd_send_signal,
};

use crate::crash::{CrashHook, CrashPoint};
pub use crate::executor::JobWait;
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use crate::firecracker::{Answer, Inspector, Query, collect_root};
use crate::fixture::FixtureExecutor;
use crate::guestlink::guest_text;
use crate::jail;
use crate::job::{JobDir, JobRequest, JobState, Mailbox, WorkerConfig};
use crate::outcomes;
use crate::supervisor::{SupervisorCmd, group_in_session, proc_stats};

/// How often a job directory is looked at while waiting.
const POLL: Duration = Duration::from_millis(25);
/// How long past its lease a job may take to die on its own before it is fenced.
const GRACE_MS: i64 = 5_000;
/// How long a fenced job gets to stop cooperatively (on the cancel marker) before the
/// controller kills its processes.
const FENCE_GRACE_MS: i64 = 2_000;
/// How long the lock may take to come free after the kills.
const KILL_SETTLE_MS: i64 = 2_000;
/// The longest lease any job of a kind other than an agent session may honestly have: no
/// configuration may use an effect timeout above it, so it must stay >= the largest
/// `EffectTimeouts` those kinds ship. It exists only to bound a forged or corrupt
/// `lease_expiry_ms` (or a wall clock that jumped back): no wait for a job lasts longer than
/// this plus `GRACE_MS` from now. An agent session is bounded by `MAX_SESSION_TIMEOUT_MS`.
pub const MAX_EFFECT_TIMEOUT_MS: i64 = 600_000;

/// The wait bound for a job whose lease ends at `lease_expiry_ms`: its lease, cut at `clamp_ms`
/// from now, plus `GRACE_MS`.
fn bounded_lease(now: i64, lease_expiry_ms: i64, clamp_ms: i64) -> i64 {
    lease_expiry_ms
        .min(now.saturating_add(clamp_ms))
        .saturating_add(GRACE_MS)
}

/// A session's configured timeout, never above `MAX_SESSION_TIMEOUT_MS`.
fn session_timeout(configured: Duration) -> Duration {
    configured.min(Duration::from_millis(MAX_SESSION_TIMEOUT_MS as u64))
}

/// How many times each effect kind was really executed, shared across executor instances
/// so it survives a simulated restart.
#[derive(Debug, Clone, Default)]
pub struct ExecCounts(Arc<[AtomicUsize; 9]>);

fn slot(tag: &str) -> usize {
    match tag {
        "read_snapshot" => 0,
        "apply_patch" => 1,
        "run_verification" => 2,
        "model_call" => 4,
        "list_files" => 5,
        "read_file" => 6,
        "analyze_snapshot" => 7,
        "run_agent_session" => 8,
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

/// What the controller makes of a guest's patch-state answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchVerdict {
    NotApplied,
    /// Applied; the workspace is now at this digest.
    Applied(Digest),
    Unknown,
}

/// The host's check on a guest's patch-state answer: `NotApplied` only with the base digest,
/// `Applied` only with a digest that is not the base; anything else (a missing digest, a
/// state that contradicts its digest, `unknown`) is `Unknown`, never `NotApplied`.
pub fn patch_verdict(
    state: PatchStateKind,
    digest: Option<Digest>,
    expected_base: Digest,
) -> PatchVerdict {
    match (state, digest) {
        (PatchStateKind::NotApplied, Some(d)) if d == expected_base => PatchVerdict::NotApplied,
        (PatchStateKind::Applied, Some(d)) if d != expected_base => PatchVerdict::Applied(d),
        _ => PatchVerdict::Unknown,
    }
}

/// How the controller looks at a task's workspace without running a job.
#[allow(clippy::large_enum_variant, reason = "one per executor, built once")]
pub enum Reconciler {
    /// The workspace is a host directory the controller reads directly.
    Host(FixtureExecutor),
    /// The workspace is a block image only the guest mounts: an inspection boot.
    Firecracker(Inspector),
}

impl Reconciler {
    /// Whether `req` (a patch) took effect. Firecracker: `NotApplied` only when the guest
    /// reports the base digest, `Applied` only with a digest other than the base (the
    /// guest reverted the patch on a copy and got the base back); anything else, and any
    /// inspection failure, is `Unknown`, never `NotApplied`.
    pub async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        let inspector = match self {
            Reconciler::Host(fixture) => return fixture.reconcile(req, ctx).await,
            Reconciler::Firecracker(inspector) => inspector,
        };
        let EffectKind::ApplyPatch { expected_base } = req.kind else {
            return Reconciliation::Unknown;
        };
        let (inspector, task, patch) =
            (inspector.clone(), req.task_id.clone(), req.payload.clone());
        let answer = tokio::task::spawn_blocking(move || {
            inspector.query(
                &task,
                Query::PatchState {
                    expected_base,
                    patch,
                },
            )
        })
        .await
        .unwrap_or_else(|e| Err(format!("workspace inspection failed: {e}")));
        let state = match answer {
            Ok(Answer::PatchState(state)) => state,
            Ok(Answer::Digest(_)) => {
                tracing::warn!(effect_id = %req.effect_id, "inspection answered a digest to a patch-state query");
                return Reconciliation::Unknown;
            }
            Err(e) => {
                tracing::warn!(effect_id = %req.effect_id, error = %e, "patch cannot be reconciled");
                return Reconciliation::Unknown;
            }
        };
        match patch_verdict(state.state, state.workspace_digest, expected_base) {
            PatchVerdict::NotApplied => Reconciliation::NotApplied,
            PatchVerdict::Applied(d) => {
                Reconciliation::Applied(outcomes::patch_applied(req, ctx, state.paths, d))
            }
            PatchVerdict::Unknown => {
                let reason = guest_text(state.reason.as_deref().unwrap_or(""));
                let (kind, digest) = (state.state, state.workspace_digest);
                tracing::warn!(effect_id = %req.effect_id, ?kind, ?digest, reason, "patch cannot be reconciled");
                Reconciliation::Unknown
            }
        }
    }

    /// The task's workspace digest, or why it cannot be had. Never `None`: either kind can
    /// always tell or say why not.
    pub fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        match self {
            Reconciler::Host(fixture) => fixture.current_workspace(task),
            Reconciler::Firecracker(inspector) => {
                Some(match inspector.query(task, Query::Digest) {
                    Ok(Answer::Digest(d)) => Ok(d),
                    Ok(Answer::PatchState(_)) => Err(
                        "workspace inspection failed: a patch state answered a digest query".into(),
                    ),
                    Err(e) => Err(e),
                })
            }
        }
    }
}

pub struct SupervisedExecutor {
    jobs_root: PathBuf,
    supervisor: SupervisorCmd,
    worker: WorkerConfig,
    timeouts: EffectTimeouts,
    counts: ExecCounts,
    crash: Option<CrashHook>,
    extra_env: Vec<(String, String)>,
    /// Answers `reconcile` and `current_workspace`, read-only: the host workspace directly
    /// (host workers), or an inspection boot (Firecracker); none for scripted workers.
    reconciler: Option<Reconciler>,
    /// `MAX_EFFECT_TIMEOUT_MS` except in tests; for every kind but an agent session.
    max_lease_clamp_ms: i64,
    /// `MAX_SESSION_TIMEOUT_MS`: the wait bound of an agent session's job.
    max_session_clamp_ms: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Waits until every job in `jobs` is settled or the clock reaches `until_ms`; returns
/// whether they all are.
async fn wait_dead(jobs: &[JobDir], until_ms: i64) -> bool {
    loop {
        if jobs.iter().all(settled) {
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
    if r.effect_id == req.effect_id
        && r.attempt_id == ctx.attempt_id
        && r.lease_generation == ctx.lease_generation
    {
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

/// Whether `pid` is the supervisor of `job`: its stdin is this job's very `lock` file (the
/// supervisor holds the lock through fd 0 for its whole life) and its command line ends with
/// `run <job_dir>`. `status.json` is worker-writable, so the pid it names proves nothing.
fn is_job_supervisor(pid: Pid, job: &JobDir) -> bool {
    let proc = PathBuf::from(format!("/proc/{}", pid.as_raw_nonzero()));
    let same_lock = match (
        fs::metadata(proc.join("fd/0")),
        fs::metadata(job.path.join("lock")),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    same_lock && runs_job(pid, job, b"run")
}

/// Whether `pid`'s command line ends with `<verb> <job_dir>` for `job`'s directory.
fn runs_job(pid: Pid, job: &JobDir, verb: &[u8]) -> bool {
    let proc = PathBuf::from(format!("/proc/{}", pid.as_raw_nonzero()));
    let Ok(cmdline) = fs::read(proc.join("cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline
        .strip_suffix(b"\0")
        .unwrap_or(&cmdline)
        .split(|b| *b == 0)
        .collect();
    let [.., found, dir] = args.as_slice() else {
        return false;
    };
    if *found != verb {
        return false;
    }
    // Relative to the supervisor's working directory, if it was launched with a relative path.
    let dir = Path::new(OsStr::from_bytes(dir));
    let dir = match fs::read_link(proc.join("cwd")) {
        Ok(cwd) => cwd.join(dir),
        Err(_) if dir.is_absolute() => dir.to_path_buf(),
        Err(_) => return false,
    };
    matches!((fs::canonicalize(dir), fs::canonicalize(&job.path)), (Ok(a), Ok(b)) if a == b)
}

/// SIGKILLs what `job` recorded, once the recorded supervisor pid is proven to be the job's
/// own supervisor: its worker group and the `groups` entries that live in the supervisor's
/// session (`groups` is worker-writable; a forged entry must not reach any other process),
/// then the supervisor itself, which a stopped supervisor cannot do for us. The supervisor
/// is signalled through a pidfd opened before the check, so a reused pid is never hit.
fn kill_job(job: &JobDir) {
    let Some(status) = job.read_status() else {
        tracing::warn!(job = %job.path.display(), "no status: no recorded process to kill");
        return;
    };
    let Some(supervisor) = status
        .supervisor_pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(signalable)
    else {
        tracing::warn!(job = %job.path.display(), pid = ?status.supervisor_pid, "no usable supervisor pid");
        return;
    };
    let pidfd = match pidfd_open(supervisor, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(job = %job.path.display(), error = %e, "the recorded supervisor is gone");
            return;
        }
    };
    if !is_job_supervisor(supervisor, job) || getsid(Some(supervisor)).ok() != Some(supervisor) {
        tracing::warn!(job = %job.path.display(), pid = supervisor.as_raw_nonzero().get(), "the recorded pid is not this job's supervisor; not signalling");
        return;
    }
    let groups = status.worker_pgid.into_iter().chain(job.groups());
    for pgid in groups {
        let Some(group) = signalable(pgid) else {
            continue;
        };
        match group_in_session(group, supervisor) {
            Some(true) => {
                let _ = kill_process_group(group, Signal::KILL);
            }
            Some(false) => {
                tracing::warn!(job = %job.path.display(), pgid, "not killing a group outside the job's session")
            }
            None => {} // nothing of the group is left
        }
    }
    tracing::warn!(job = %job.path.display(), pid = supervisor.as_raw_nonzero().get(), "killing the supervisor");
    let _ = pidfd_send_signal(&pidfd, Signal::KILL);
    // The supervisor's session holds only the job: whatever the worker started in a group it
    // had no time to record dies too. Its members keep the session id from being reused.
    match proc_stats() {
        Ok(stats) => kill_session(supervisor, &stats),
        Err(e) => {
            tracing::warn!(job = %job.path.display(), error = %e, "cannot scan /proc for the job's session")
        }
    }
}

/// SIGKILLs the live members of session `sid` found in `stats` (one `/proc` snapshot), each
/// through a pidfd opened before its session is checked again, so a pid reused since the
/// snapshot is never hit. Never our own session, and never the leader `sid` itself: a
/// supervisor is only ever signalled through the pidfd its verification opened.
fn kill_session(sid: Pid, stats: &[(i32, [String; 4])]) {
    if getsid(None).ok() == Some(sid) {
        return;
    }
    let raw = sid.as_raw_nonzero().get();
    let session_field = raw.to_string();
    for (pid, [state, _, _, session]) in stats {
        if *session != session_field || state == "Z" || *pid == raw {
            continue;
        }
        let Some(pid) = signalable(*pid) else {
            continue;
        };
        let Ok(fd) = pidfd_open(pid, PidfdFlags::empty()) else {
            continue;
        };
        if getsid(Some(pid)).ok() == Some(sid) {
            let _ = pidfd_send_signal(&fd, Signal::KILL);
        }
    }
}

/// Whether nothing of `job` can run any more. A job is dead when its lock is free, but a
/// job whose supervisor was killed may have left its worker and checks running in the
/// supervisor's (now leaderless) session; those are killed here, and the job is settled
/// once none is left. Only a session proven to be the job's is touched: the one a live
/// process running `worker <job_dir>` belongs to, whose leader is gone. Processes left in
/// the session `status.json` names (worker-writable) but not tied to the job that way keep
/// the job unsettled: it is never reported settled while something of it may still run.
fn settled(job: &JobDir) -> bool {
    if !job.is_dead() {
        return false;
    }
    let status = job.read_status();
    if job.read_receipt().is_some()
        || matches!(
            status.as_ref().map(|s| s.state),
            Some(JobState::Exited | JobState::Killed)
        )
    {
        // The supervisor wrote these only after every process of the job was reaped.
        return true;
    }
    let stats = match proc_stats() {
        Ok(stats) => stats,
        Err(e) => {
            tracing::warn!(job = %job.path.display(), error = %e, "cannot scan /proc for leftovers; assuming some");
            return false;
        }
    };
    let live = |pid: i32| {
        stats
            .iter()
            .any(|(p, [state, ..])| *p == pid && state != "Z")
    };
    let members = |sid: i32| {
        let sid = sid.to_string();
        stats
            .iter()
            .filter(move |(_, [state, _, _, session])| *session == sid && state != "Z")
            .map(|(p, _)| *p)
    };
    let own_session = getsid(None).ok().map(|s| s.as_raw_nonzero().get());
    let workers: BTreeSet<i32> = stats
        .iter()
        .filter(|(pid, [state, ..])| {
            state != "Z" && Pid::from_raw(*pid).is_some_and(|p| runs_job(p, job, b"worker"))
        })
        .filter_map(|(_, [.., session])| session.parse().ok())
        .collect();
    let recorded = status
        .and_then(|s| s.supervisor_pid)
        .and_then(|p| i32::try_from(p).ok());
    let mut clean = true;
    for sid in workers.iter().copied().chain(recorded) {
        if sid <= 1 || Some(sid) == own_session || members(sid).next().is_none() {
            continue;
        }
        if live(sid) {
            // A live leader cannot be the job's supervisor (it would hold the lock), so
            // this session is someone else's, or the job's in a way we cannot explain.
            if workers.contains(&sid) {
                clean = false;
                tracing::warn!(job = %job.path.display(), sid, "the job's worker lives in a session with a live leader; not killing");
            }
            continue;
        }
        clean = false;
        match Pid::from_raw(sid) {
            Some(session) if workers.contains(&sid) => {
                tracing::warn!(job = %job.path.display(), sid, "killing what the job's dead supervisor left running");
                kill_session(session, &stats);
            }
            _ => {
                tracing::warn!(job = %job.path.display(), sid, "processes left in the recorded session cannot be tied to the job; not killing")
            }
        }
    }
    clean
}

/// Every job of `effect` under `jobs_root`. A jobs root that does not exist holds no job (it
/// was removed from outside, or nothing ever ran); any other failure to list it is an error,
/// never "no job".
fn jobs_of(jobs_root: &Path, effect: &EffectId) -> io::Result<Vec<JobDir>> {
    match JobDir::list(jobs_root, effect) {
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && fs::symlink_metadata(jobs_root)
                    .is_err_and(|m| m.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(Vec::new())
        }
        listed => listed,
    }
}

/// Asks the live jobs of `effects` under `jobs_root` to stop by dropping their `cancel`
/// marker; the supervisors kill their workers (and VMs) within a poll interval. Needs no
/// worker configuration, so a controller whose worker cannot run (preflight, jail) can still
/// stop what is running. Returns how many markers were dropped.
pub fn cancel_jobs_in(jobs_root: &Path, effects: &[EffectId]) -> usize {
    let mut dropped = 0;
    for effect in effects {
        let jobs = match jobs_of(jobs_root, effect) {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(effect_id = %effect, error = %e, "cannot list the effect's jobs to cancel them");
                continue;
            }
        };
        for job in jobs.iter().filter(|j| !j.is_dead()) {
            match job.drop_cancel() {
                Ok(()) => dropped += 1,
                Err(e) => {
                    tracing::warn!(job = %job.path.display(), error = %e, "cannot drop the cancel marker")
                }
            }
        }
    }
    dropped
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
            WorkerConfig::Host(h) => Some(Reconciler::Host(
                FixtureExecutor::new(
                    h.snapshot_dir.clone(),
                    h.profile_dir.clone(),
                    h.work_root.clone(),
                )
                .with_verify_timeout(Duration::from_secs(h.verify_timeout_secs))
                .with_pinned_profile(h.profile_digest),
            )),
            WorkerConfig::Scripted(_) => None,
            WorkerConfig::Firecracker(cfg) => {
                // `<home>/jobs` ⇒ `<home>/inspect`.
                let inspect_root = jobs_root.parent().unwrap_or(&jobs_root).join("inspect");
                Some(Reconciler::Firecracker(Inspector::new(
                    cfg.clone(),
                    inspect_root,
                )))
            }
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
            max_lease_clamp_ms: MAX_EFFECT_TIMEOUT_MS,
            max_session_clamp_ms: MAX_SESSION_TIMEOUT_MS,
        })
    }

    /// Test seam: replaces `MAX_EFFECT_TIMEOUT_MS` as the clamp on lease-bounded waits.
    pub fn with_max_lease_clamp(mut self, clamp: Duration) -> SupervisedExecutor {
        self.max_lease_clamp_ms = millis(clamp);
        self
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

    /// Sets `key` in the supervisor's (and so the worker's) environment, and in the
    /// environment the inspector treats as its own.
    pub fn with_env(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> SupervisedExecutor {
        let (key, value) = (key.into(), value.into());
        if let Some(Reconciler::Firecracker(inspector)) = &mut self.reconciler {
            inspector.push_env(key.clone(), value.clone());
        }
        self.extra_env.push((key, value));
        self
    }

    pub fn counts(&self) -> &ExecCounts {
        &self.counts
    }

    fn timeout(&self, kind: &EffectKind) -> Duration {
        match kind {
            EffectKind::RunVerification => self.timeouts.verification,
            EffectKind::RunAgentSession { .. } => session_timeout(self.timeouts.session),
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
            // The controller holds secrets (the model API key): the supervisor, and the
            // worker that inherits from it, get exactly `PATH` plus the explicit extras.
            cmd.env_clear();
            if let Some(path) = std::env::var_os("PATH") {
                cmd.env("PATH", path);
            }
            for (k, v) in &self.extra_env {
                cmd.env(k, v);
            }
            cmd.spawn()
            // `cmd`, and with it the controller's copy of the lock, is dropped here.
        };
        let mut child = spawned?;
        let reaper = std::thread::Builder::new()
            .name("supervisor-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        if let Err(e) = reaper {
            tracing::warn!(job = %job.path.display(), error = %e, "cannot reap the supervisor; it stays a zombie");
        }
        Ok(())
    }

    /// Every job of `effect`. A jobs root that does not exist holds no job (it was removed
    /// from outside); any other failure to list it is an error, never "no job".
    fn jobs(&self, effect: &EffectId) -> io::Result<Vec<JobDir>> {
        jobs_of(&self.jobs_root, effect)
    }

    /// When to stop waiting for a job whose lease ends at `lease_expiry_ms`: its lease plus
    /// `GRACE_MS`, but never later than the longest honest lease from `now`
    /// (`MAX_EFFECT_TIMEOUT_MS`) plus `GRACE_MS`, so a forged or corrupt lease cannot stall
    /// the wait.
    fn lease_bound(&self, kind: &EffectKind, now: i64, lease_expiry_ms: i64) -> i64 {
        let clamp = match kind {
            EffectKind::RunAgentSession { .. } => self.max_session_clamp_ms,
            _ => self.max_lease_clamp_ms,
        };
        bounded_lease(now, lease_expiry_ms, clamp)
    }

    /// The job's `request.json`. A Firecracker job gets a fresh attempt token of its own.
    fn job_request(
        &self,
        req: &EffectRequest,
        ctx: &AttemptCtx,
        lease_expiry_ms: i64,
    ) -> JobRequest {
        let mut worker = self.worker.clone();
        if let WorkerConfig::Firecracker(cfg) = &mut worker {
            cfg.attempt_token = mint_attempt_token();
        }
        JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms,
            task_deadline_ms: req.deadline_ts.saturating_mul(1000),
            worker,
        }
    }

    /// Collects the jail a settled Firecracker job left (its worker died without collecting
    /// it: a supervisor SIGKILL, a lease kill). Only ever called once 3a has proven the job
    /// settled; a failure is a warning.
    fn collect_jails(&self, jobs: &[JobDir]) {
        let WorkerConfig::Firecracker(cfg) = &self.worker else {
            return;
        };
        for job in jobs {
            if let Err(e) = jail::collect(&job.path, collect_root(cfg)) {
                tracing::warn!(job = %job.path.display(), error = %e, "the job's jail was not collected");
            }
        }
    }

    async fn reconcile_in_place(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        match &self.reconciler {
            Some(r) => r.reconcile(req, ctx).await,
            None => Reconciliation::Unknown,
        }
    }

    /// Keeps a reconciled outcome as the receipt of a job directory of its own (attempt
    /// `ctx`, never launched), so a later recovery publishes it instead of reconciling again.
    /// A failure is logged: without the copy, recovery reconciles again.
    fn retain(&self, req: &EffectRequest, ctx: &AttemptCtx, out: &ExecOutcome) {
        let kept = JobDir::create(&self.jobs_root, &self.job_request(req, ctx, now_ms())).and_then(
            |(job, lock)| {
                // The receipt goes in while the lock is held, so the job is never seen dead
                // without it.
                let written = job.write_receipt(out);
                drop(lock);
                written
            },
        );
        if let Err(e) = kept {
            tracing::warn!(effect_id = %req.effect_id, error = %e, "could not retain the reconciled outcome");
        }
    }

    /// The outcome of a job that is dead without a receipt (the kill-receipt rule).
    async fn without_receipt(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind.retry_policy() {
            RetryPolicy::Retry => {
                ExecOutcome::failure(req, ctx, "supervisor died without a receipt")
            }
            RetryPolicy::ReconcileThenRetry => match self.reconcile_in_place(req, ctx).await {
                Reconciliation::Applied(out) => out,
                Reconciliation::NotApplied => {
                    ExecOutcome::failure(req, ctx, "patch provably not applied")
                }
                Reconciliation::Unknown => ExecOutcome::unresolved(
                    req,
                    ctx,
                    "the job died without a receipt and reconciliation cannot tell whether it took effect",
                ),
            },
            RetryPolicy::NoRetry => ExecOutcome::unresolved(
                req,
                ctx,
                "the job died without a receipt and its kind is never retried",
            ),
            RetryPolicy::ForfeitThenRetry => ExecOutcome::unresolved(
                req,
                ctx,
                "the job died without a receipt; its kind is forfeited, never retried",
            ),
        }
    }

    /// Asks the live jobs of `effects` to stop by dropping their `cancel` marker; the
    /// supervisors kill their workers within a poll interval. Nothing is waited for: the
    /// caller (or recovery) reads what the jobs leave. Returns how many markers were dropped.
    pub fn cancel_jobs(&self, effects: &[EffectId]) -> usize {
        cancel_jobs_in(&self.jobs_root, effects)
    }

    /// Fences every job in `jobs` that is not dead: drops `cancel`, gives the job
    /// `FENCE_GRACE_MS` to stop, then kills its recorded processes. Returns whether every
    /// job is dead afterwards.
    pub async fn fence_jobs(&self, jobs: &[JobDir]) -> bool {
        let live: Vec<JobDir> = jobs.iter().filter(|j| !settled(j)).cloned().collect();
        if live.is_empty() {
            self.collect_jails(jobs);
            return true;
        }
        for job in &live {
            if let Err(e) = job.drop_cancel() {
                tracing::warn!(job = %job.path.display(), error = %e, "cannot drop the cancel marker");
            }
        }
        if wait_dead(&live, now_ms().saturating_add(FENCE_GRACE_MS)).await {
            self.collect_jails(jobs);
            return true;
        }
        for job in live.iter().filter(|j| !settled(j)) {
            kill_job(job);
        }
        let dead = wait_dead(&live, now_ms().saturating_add(KILL_SETTLE_MS)).await;
        if dead {
            self.collect_jails(jobs);
        } else {
            tracing::warn!(jobs = ?live.iter().map(|j| j.path.display().to_string()).collect::<Vec<_>>(), "jobs survived the fence");
            // Only the jobs proven settled; the others may still have a VM in their jail.
            let settled_jobs: Vec<JobDir> = jobs.iter().filter(|j| settled(j)).cloned().collect();
            self.collect_jails(&settled_jobs);
        }
        dead
    }

    /// Waits for every attempt of `effect` to die. A job whose request can be read is waited
    /// for until its OWN lease plus `GRACE_MS`, whatever `bound` says: recovery never fences
    /// a job inside its lease, even when this executor's timeouts are shorter than the ones
    /// the job was launched under. Only a lease beyond any honest one is cut short, at
    /// `MAX_EFFECT_TIMEOUT_MS` from now (see [`SupervisedExecutor::lease_bound`]). `bound` (from now) limits only the wait for a job whose
    /// request cannot be read, which has no known lease. A dead job's leftovers get
    /// `KILL_SETTLE_MS` to die once killed. A jobs root that cannot be listed is
    /// `StillAlive`: it proves nothing about the jobs in it.
    pub async fn wait_for_job(&self, effect: &EffectId, bound: Duration) -> JobWait {
        let jobs = match self.jobs(effect) {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::warn!(effect_id = %effect, root = %self.jobs_root.display(), error = %e, "cannot list the effect's jobs; treating them as alive");
                return JobWait::StillAlive;
            }
        };
        let now = now_ms();
        let unknown_lease = now.saturating_add(millis(bound));
        let until = jobs
            .iter()
            .filter(|j| !settled(j))
            .map(|j| match j.is_dead() {
                true => now.saturating_add(KILL_SETTLE_MS),
                false => j.request().map_or(unknown_lease, |r| {
                    self.lease_bound(&r.kind, now, r.lease_expiry_ms)
                }),
            })
            .max()
            .unwrap_or(now);
        if !wait_dead(&jobs, until).await {
            return JobWait::StillAlive;
        }
        self.collect_jails(&jobs);
        match self.retained_outcome(effect) {
            Some(out) => JobWait::Receipt(Box::new(out)),
            None => JobWait::Dead,
        }
    }
}

impl Executor for SupervisedExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        let now = now_ms();
        let lease = lease_expiry_ms(
            now,
            millis(self.timeout(&req.kind)),
            req.deadline_ts.saturating_mul(1000),
        );
        if lease <= now {
            return ExecOutcome::failure(req, ctx, "deadline exceeded");
        }
        let (job, lock) = match JobDir::create(&self.jobs_root, &self.job_request(req, ctx, lease))
        {
            Ok(created) => created,
            Err(e) => {
                return ExecOutcome::failure(
                    req,
                    ctx,
                    format!("supervisor launch failed: cannot create the job: {e}"),
                );
            }
        };
        if let Err(e) = self.launch(&job, lock) {
            // Nothing runs and the lock is free again: leave no dead job without a receipt.
            if let Err(e) = fs::remove_dir_all(&job.path) {
                tracing::warn!(job = %job.path.display(), error = %e, "cannot remove the unlaunched job");
            }
            return ExecOutcome::failure(req, ctx, format!("supervisor launch failed: {e}"));
        }
        // Counted only now: a refused or failed launch executed nothing.
        self.counts.record(&req.kind);
        if self
            .crash
            .as_ref()
            .is_some_and(|h| h.check(CrashPoint::DuringExecute, Some(req.kind.tag())))
        {
            // The controller "dies" with the job running; the runner discards this outcome.
            return ExecOutcome::failure(req, ctx, "injected crash after the launch");
        }
        let jobs = std::slice::from_ref(&job);
        if !wait_dead(jobs, self.lease_bound(&req.kind, now_ms(), lease)).await
            && !self.fence_jobs(jobs).await
        {
            return ExecOutcome::unresolved(
                req,
                ctx,
                "the job outlived its lease and could not be stopped",
            );
        }
        // Settled: its jail, if its worker could not collect it, goes now, before any
        // inspection boots on the same image.
        self.collect_jails(jobs);
        match receipt_for(&job, req, ctx) {
            Some(out) => out,
            None => self.without_receipt(req, ctx).await,
        }
    }

    /// The receipt of the highest lease generation among `effect`'s jobs. A receipt flagged
    /// `unresolved` is no receipt.
    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        let jobs = match self.jobs(effect) {
            Ok(jobs) => jobs,
            Err(e) => {
                // `await_job` treats the same error as a live job, so recovery never acts
                // on this "none".
                tracing::warn!(effect_id = %effect, error = %e, "cannot list the effect's jobs");
                return None;
            }
        };
        jobs.iter()
            .filter_map(JobDir::read_receipt)
            .filter(|out| out.receipt.effect_id == *effect && !out.unresolved)
            .max_by_key(|out| out.receipt.lease_generation)
    }

    /// Reconciles in process; an `Applied` outcome is retained like a receipt.
    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        let found = self.reconcile_in_place(req, ctx).await;
        if let Reconciliation::Applied(out) = &found {
            self.retain(req, ctx, out);
        }
        found
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.reconciler
            .as_ref()
            .and_then(|r| r.current_workspace(task))
    }

    /// [`SupervisedExecutor::wait_for_job`]. A job whose request cannot be read has no known
    /// kind, so no known lease (`request.json` is the only record of one): it is waited for
    /// the longest job of any kind may run, `max_lease_clamp_ms` plus `GRACE_MS`, and never
    /// longer, whatever the session timeout is.
    async fn await_job(&self, effect: &EffectId) -> JobWait {
        let bound = self.max_lease_clamp_ms.max(0).saturating_add(GRACE_MS);
        self.wait_for_job(effect, Duration::from_millis(bound as u64))
            .await
    }

    async fn fence_job(&self, effect: &EffectId) -> bool {
        match self.jobs(effect) {
            Ok(jobs) => self.fence_jobs(&jobs).await,
            Err(e) => {
                tracing::warn!(effect_id = %effect, error = %e, "cannot list the effect's jobs to fence them");
                false
            }
        }
    }

    /// Only a Firecracker worker runs the guest's agent session.
    fn runs_agent_sessions(&self) -> bool {
        matches!(self.worker, WorkerConfig::Firecracker(_))
    }

    /// The newest attempt's job (the jobs list in ascending lease generation).
    fn session_mailbox(&self, effect: &EffectId) -> Option<Mailbox> {
        self.jobs(effect)
            .ok()?
            .last()
            .map(|job| Mailbox::new(job.session_dir()))
    }

    fn cancel_jobs(&self, effects: &[EffectId]) -> usize {
        SupervisedExecutor::cancel_jobs(self, effects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_verdict_table() {
        let (base, other) = (Digest::of(b"base"), Digest::of(b"other"));
        use PatchStateKind::{Applied, NotApplied, Unknown};
        let rows = [
            (Applied, None, PatchVerdict::Unknown),
            (Applied, Some(base), PatchVerdict::Unknown),
            (Applied, Some(other), PatchVerdict::Applied(other)),
            (NotApplied, None, PatchVerdict::Unknown),
            (NotApplied, Some(other), PatchVerdict::Unknown),
            (NotApplied, Some(base), PatchVerdict::NotApplied),
            (Unknown, None, PatchVerdict::Unknown),
            (Unknown, Some(base), PatchVerdict::Unknown),
            (Unknown, Some(other), PatchVerdict::Unknown),
        ];
        for (state, digest, want) in rows {
            assert_eq!(
                patch_verdict(state, digest, base),
                want,
                "{state:?} {digest:?}"
            );
        }
    }

    #[test]
    fn a_session_lease_is_honoured_up_to_the_session_cap_and_other_kinds_keep_theirs() {
        let now = 1_000_000_000;
        let half_hour = 1_800_000;
        let five_hours = 5 * 3_600_000;
        // A 30 min session is not clamped to the 600 s of every other kind.
        assert_eq!(
            bounded_lease(now, now + half_hour, MAX_SESSION_TIMEOUT_MS),
            now + half_hour + GRACE_MS
        );
        // A 5 h session is clamped at four hours.
        assert_eq!(
            bounded_lease(now, now + five_hours, MAX_SESSION_TIMEOUT_MS),
            now + MAX_SESSION_TIMEOUT_MS + GRACE_MS
        );
        // A non-session kind is still clamped at 600 s.
        assert_eq!(
            bounded_lease(now, now + half_hour, MAX_EFFECT_TIMEOUT_MS),
            now + MAX_EFFECT_TIMEOUT_MS + GRACE_MS
        );
    }

    #[test]
    fn a_configured_session_timeout_is_capped_at_four_hours() {
        assert_eq!(
            session_timeout(Duration::from_secs(1800)),
            Duration::from_secs(1800)
        );
        assert_eq!(
            session_timeout(Duration::from_secs(5 * 3600)),
            Duration::from_millis(MAX_SESSION_TIMEOUT_MS as u64)
        );
        assert_eq!(MAX_SESSION_TIMEOUT_MS, 14_400_000);
        assert_eq!(MAX_EFFECT_TIMEOUT_MS, 600_000);
    }
}
