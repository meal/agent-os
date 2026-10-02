//! The per-job supervisor: owns one worker for one job, enforces the job's lease, task
//! deadline and cancel marker, turns the worker's outcome into the durable receipt, and
//! exits.
//!
//! The controller launches it with the job's locked `lock` file as stdin and keeps that
//! descriptor away from everything else, so the lock is held exactly as long as the
//! supervisor lives (the worker gets null stdio). The supervisor never touches fd 0.
//!
//! Every way a job ends goes through the same shutdown: SIGKILL the worker's group, reap
//! the worker, SIGKILL every group in `groups`, then kill and reap every remaining child
//! (as child subreaper, every orphaned descendant of the job is one). Only then is the
//! receipt written, and only after the receipt the terminal status: a valid receipt makes
//! `JobDir::is_dead` true, so no process of the job may outlive it.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::RetryPolicy;
use rustix::io::Errno;
use rustix::process::{
    getpgrp, getpid, getsid, kill_process, kill_process_group, set_child_subreaper, setsid, wait, waitid, Pid,
    Signal, WaitId, WaitIdOptions, WaitOptions,
};

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome};
use crate::job::{JobDir, JobRequest, JobState, JobStatus, KillReason};
use crate::worker::{run_worker, TEST_WORKERS_ENV};

/// Overrides the 50 ms poll interval; honoured only with `AGENTOS_TEST_WORKERS=1`.
pub const POLL_ENV: &str = "AGENTOS_SUPERVISOR_POLL_MS";
/// Test-only hook, honoured only together with `AGENTOS_TEST_WORKERS=1`: exit with
/// `EXIT_BEFORE_RECEIPT_CODE` right after reading a valid `outcome.json`, before the
/// receipt is written (the Phase 2 "executed but not durable" case).
pub const EXIT_BEFORE_RECEIPT_ENV: &str = "AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT";
pub const EXIT_BEFORE_RECEIPT_CODE: i32 = 3;
/// Test-only hook, honoured only together with `AGENTOS_TEST_WORKERS=1`: the first
/// `TEST_FAILED_SCANS` scans for children fail, as if `/proc` were unreadable.
pub const PROC_SCAN_FAILS_ENV: &str = "AGENTOS_TEST_SUPERVISOR_PROC_SCAN_FAILS";
const TEST_FAILED_SCANS: u32 = 3;
static FAILED_SCANS: AtomicU32 = AtomicU32::new(0);
/// Test-only hook, honoured only together with `AGENTOS_TEST_WORKERS=1`: panic in the poll
/// loop once `<job>/panic-now` exists (the test's script creates it when its processes are
/// up), to prove a panic never frees the lock while the job lives.
pub const PANIC_AFTER_SPAWN_ENV: &str = "AGENTOS_TEST_SUPERVISOR_PANIC_AFTER_SPAWN";
pub const LOG_FILE: &str = "supervisor.log";

const DEFAULT_POLL: Duration = Duration::from_millis(50);
/// How long to let SIGKILLed children die before looking for them again; doubles while
/// nothing is reaped (a child in uninterruptible sleep can take a while).
const REAP_PAUSE: Duration = Duration::from_millis(5);
const REAP_PAUSE_MAX: Duration = Duration::from_millis(500);

/// How the supervisor starts its worker: `program prefix_args… worker <job_dir>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorCmd {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Best effort and never panics (unlike `eprintln!`): a full or failing `supervisor.log`
/// must not take the supervisor down while its worker lives.
fn log(msg: impl std::fmt::Display) {
    let _ = writeln!(io::stderr(), "[{}] supervisor {}: {msg}", now_ms(), std::process::id());
}

fn env_on(var: &str) -> bool {
    std::env::var(var).as_deref() == Ok("1")
}

/// A test hook is honoured only together with `AGENTOS_TEST_WORKERS=1`.
fn test_hook(var: &str) -> bool {
    env_on(TEST_WORKERS_ENV) && env_on(var)
}

fn poll_interval() -> Duration {
    if !env_on(TEST_WORKERS_ENV) {
        return DEFAULT_POLL;
    }
    std::env::var(POLL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_POLL, Duration::from_millis)
}

/// Checked in this order, so the reason is deterministic when several apply.
fn kill_reason(job: &JobDir, req: &JobRequest, now: i64) -> Option<KillReason> {
    if job.cancel_requested() {
        Some(KillReason::Cancel)
    } else if req.task_deadline_ms != 0 && now >= req.task_deadline_ms {
        Some(KillReason::Deadline)
    } else if now >= req.lease_expiry_ms {
        Some(KillReason::Lease)
    } else {
        None
    }
}

fn kill_text(reason: KillReason) -> &'static str {
    match reason {
        KillReason::Lease => "lease expired",
        KillReason::Deadline => "deadline exceeded",
        KillReason::Cancel => "cancelled",
    }
}

fn failure(req: &JobRequest, reason: &str) -> ExecOutcome {
    let effect = EffectRequest {
        effect_id: req.effect_id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        payload: req.payload.clone(),
        contract: req.contract.clone(),
        deadline_ts: req.task_deadline_ms / 1000,
    };
    let ctx = AttemptCtx {
        attempt_id: req.attempt_id.clone(),
        lease_generation: req.lease_generation,
        worker: "supervisor".into(),
    };
    ExecOutcome::failure(&effect, &ctx, reason)
}

/// The receipt for a job that ended without a valid outcome: a failure for effects that
/// leave no lasting change, and none for the others, which may or may not have happened
/// (only reconciliation can tell).
fn fallback_receipt(req: &JobRequest, reason: &str) -> Option<ExecOutcome> {
    match req.kind.retry_policy() {
        RetryPolicy::Retry => Some(failure(req, reason)),
        RetryPolicy::ReconcileThenRetry | RetryPolicy::NoRetry => None,
    }
}

/// The worker's outcome, if it is intact (`read_outcome` checks the digest) and belongs to
/// this very attempt.
fn valid_outcome(job: &JobDir, req: &JobRequest) -> Option<ExecOutcome> {
    let out = job.read_outcome()?;
    let r = &out.receipt;
    if r.effect_id == req.effect_id && r.attempt_id == req.attempt_id && r.lease_generation == req.lease_generation {
        Some(out)
    } else {
        log(format_args!(
            "outcome.json is for {} attempt {} generation {}, not this job",
            r.effect_id, r.attempt_id, r.lease_generation
        ));
        None
    }
}

/// `(state, ppid, pgrp, session)` of every process in `/proc`, as raw strings.
fn proc_stats() -> io::Result<Vec<(i32, [String; 4])>> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        // Fields after the command name, which may itself contain spaces and parentheses.
        let Some(rest) = stat.rfind(')').and_then(|i| stat.get(i + 1..)) else { continue };
        let mut fields = rest.split_whitespace().map(str::to_string);
        if let (Some(state), Some(ppid), Some(pgrp), Some(session)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            found.push((pid, [state, ppid, pgrp, session]));
        }
    }
    Ok(found)
}

/// Whether group `pgid` belongs to our session. Its leader answers directly; a group whose
/// leader is gone answers through any member (a group never spans sessions). Unknown means
/// no: `groups` is written by the worker, and a forged entry must never reach a process
/// outside the job, such as the controller's.
fn in_our_session(pgid: Pid, sid: Pid) -> Option<bool> {
    if let Ok(leader_sid) = getsid(Some(pgid)) {
        return Some(leader_sid == sid);
    }
    let (pgid, sid) = (pgid.as_raw_nonzero().get().to_string(), sid.as_raw_nonzero().get().to_string());
    let Ok(stats) = proc_stats() else { return Some(false) };
    // `None`: no member is left, so there is nothing to kill.
    stats.iter().find(|(_, [_, _, pgrp, _])| *pgrp == pgid).map(|(_, [.., session])| *session == sid)
}

/// SIGKILLs recorded group `pgid`: never init's (1, which would mean "everyone"), never
/// our own, never one outside our session.
fn kill_recorded_group(pgid: i32, sid: Pid) {
    if pgid > 1
        && pgid != getpgrp().as_raw_nonzero().get()
        && let Some(p) = Pid::from_raw(pgid)
    {
        match in_our_session(p, sid) {
            Some(true) => {
                let _ = kill_process_group(p, Signal::KILL);
            }
            Some(false) => log(format_args!("not killing recorded group {pgid}: not in our session")),
            None => {}
        }
    }
}

/// SIGKILLs every live child of this process. A child's pid cannot be reused before we
/// reap it, so nothing else can be hit.
fn kill_children() -> io::Result<()> {
    if test_hook(PROC_SCAN_FAILS_ENV) && FAILED_SCANS.fetch_add(1, Ordering::Relaxed) < TEST_FAILED_SCANS {
        return Err(io::Error::other("test hook: /proc scan fails"));
    }
    let me = std::process::id().to_string();
    for (pid, [state, ppid, ..]) in proc_stats()? {
        if ppid == me
            && state != "Z"
            && let Some(p) = Pid::from_raw(pid)
        {
            let _ = kill_process(p, Signal::KILL);
        }
    }
    Ok(())
}

/// Reaps children until none is left, killing any that still live. A failed `/proc` scan
/// is retried with the same backoff: we never return while a child exists, so the lock
/// stays held for as long as any process of the job lives.
fn reap_all() -> io::Result<()> {
    let (mut pause, mut scan_failing) = (REAP_PAUSE, false);
    loop {
        match wait(WaitOptions::NOHANG) {
            Ok(Some(_)) => pause = REAP_PAUSE,
            Err(Errno::INTR) => {}
            Ok(None) => {
                match kill_children() {
                    Err(e) if !scan_failing => {
                        log(format_args!("cannot scan /proc ({e}); retrying"));
                        scan_failing = true;
                    }
                    Err(_) => {}
                    Ok(()) => scan_failing = false,
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(REAP_PAUSE_MAX);
            }
            Err(Errno::CHILD) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Whether the worker has exited, WITHOUT reaping it: while it is an unreaped zombie its
/// pid, and so its process group id, cannot be reused, which keeps the group kill exact.
fn exited(worker: Pid) -> io::Result<bool> {
    match waitid(WaitId::Pid(worker), WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT) {
        Ok(found) => Ok(found.is_some()),
        Err(Errno::INTR) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

struct Supervisor<'a> {
    job: &'a JobDir,
    req: JobRequest,
    pid: u32,
    sid: Pid,
    /// Set once the worker is reaped: from then on its pid (and group id) may be reused.
    worker_reaped: Cell<bool>,
}

impl Supervisor<'_> {
    fn status(&self, state: JobState, reason: Option<KillReason>, worker_pgid: Option<i32>) -> io::Result<()> {
        self.job.write_status(&JobStatus {
            state,
            reason,
            supervisor_pid: Some(self.pid),
            worker_pgid,
            updated_ms: now_ms(),
        })
    }

    /// The shutdown every job goes through once its worker exited or must die.
    fn stop(&self, child: &mut Child, worker: Pid) -> io::Result<()> {
        if !self.worker_reaped.get() {
            // The worker is not reaped yet, so its pid and group id still name only it and
            // its group (the direct kill covers a worker that left its group).
            let _ = kill_process_group(worker, Signal::KILL);
            let _ = kill_process(worker, Signal::KILL);
            child.wait()?;
            self.worker_reaped.set(true);
        }
        // Read only now: the worker can no longer record a group.
        for pgid in self.job.groups() {
            kill_recorded_group(pgid, self.sid);
        }
        // Catches what left every recorded group (setsid, or a group the worker had no
        // time to record): orphans are reparented to us.
        reap_all()
    }

    /// The KILL-RECEIPT RULE: a valid outcome is published, otherwise the fallback
    /// receipt (if any); then the terminal status.
    fn finish_killed(&self, reason: KillReason, worker_pgid: Option<i32>) -> io::Result<JobState> {
        log(format_args!("killing the job: {}", kill_text(reason)));
        let receipt = valid_outcome(self.job, &self.req).or_else(|| fallback_receipt(&self.req, kill_text(reason)));
        if let Some(receipt) = receipt {
            self.job.write_receipt(&receipt)?;
        }
        self.status(JobState::Killed, Some(reason), worker_pgid)?;
        Ok(JobState::Killed)
    }

    fn supervise(&self, child: &mut Child, worker: Pid, poll: Duration) -> io::Result<JobState> {
        let pgid = Some(worker.as_raw_nonzero().get());
        self.status(JobState::Running, None, pgid)?;
        loop {
            if test_hook(PANIC_AFTER_SPAWN_ENV) && self.job.path.join("panic-now").exists() {
                panic!("test hook: panicking after the spawn");
            }
            if let Some(reason) = kill_reason(self.job, &self.req, now_ms()) {
                self.stop(child, worker)?;
                return self.finish_killed(reason, pgid);
            }
            if exited(worker)? {
                break;
            }
            std::thread::sleep(poll);
        }
        let outcome = valid_outcome(self.job, &self.req);
        if outcome.is_some() && test_hook(EXIT_BEFORE_RECEIPT_ENV) {
            log("test hook: exiting before the receipt");
            std::process::exit(EXIT_BEFORE_RECEIPT_CODE);
        }
        self.stop(child, worker)?;
        let receipt = outcome.or_else(|| {
            log("the worker exited without a valid outcome");
            fallback_receipt(&self.req, "worker produced an invalid outcome")
        });
        if let Some(receipt) = receipt {
            self.job.write_receipt(&receipt)?;
        }
        self.status(JobState::Exited, None, pgid)?;
        Ok(JobState::Exited)
    }
}

/// Supervises `job` to a terminal state. Blocking: the whole supervisor is one thread that
/// polls every 50 ms. Must run in a process whose stdin is the job's locked `lock` file.
/// `Err` means the supervisor itself failed; any worker it started is killed first.
pub fn run_supervisor(job: &JobDir, worker_cmd: &SupervisorCmd) -> io::Result<JobState> {
    let req = job
        .request()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot read {}: {e}", job.path.join("request.json").display())))?;
    // EPERM: `main_run` already made us a session leader, or the launcher made us a group
    // leader. The second case is refused below.
    match setsid() {
        Ok(_) | Err(Errno::PERM) => {}
        Err(e) => return Err(e.into()),
    }
    // The session guard trusts our session to hold only the job: staying in the launcher's
    // session would let a forged `groups` entry reach the controller's own groups.
    if getsid(None)? != getpid() {
        return Err(io::Error::other(
            "not a session leader: launch the supervisor with a plain spawn (no process_group/setsid by the parent)",
        ));
    }
    set_child_subreaper(Some(getpid()))?;
    if !job.lock_held() {
        return Err(io::Error::other("the job lock is not held: launch the supervisor with the locked lock file as stdin"));
    }
    let sid = getsid(None)?;
    let sup = Supervisor { job, req, pid: std::process::id(), sid, worker_reaped: Cell::new(false) };
    sup.status(JobState::Starting, None, None)?;
    if let Some(reason) = kill_reason(job, &sup.req, now_ms()) {
        // Nothing was spawned, so there is nothing to kill.
        return sup.finish_killed(reason, None);
    }
    let mut child = Command::new(&worker_cmd.program)
        .args(&worker_cmd.prefix_args)
        .arg("worker")
        .arg(&job.path)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let worker = Pid::from_raw(child.id() as i32).ok_or_else(|| io::Error::other("worker has no pid"))?;
    // From here on a worker may be alive, and the lock is released when we exit: neither
    // an error nor a panic may end the supervisor before everything of the job is dead.
    let poll = poll_interval();
    let failed = match panic::catch_unwind(AssertUnwindSafe(|| sup.supervise(&mut child, worker, poll))) {
        Ok(Ok(state)) => return Ok(state),
        Ok(Err(e)) => e,
        Err(_) => io::Error::other("the supervisor panicked"),
    };
    log(format_args!("failed ({failed}); stopping the job"));
    match panic::catch_unwind(AssertUnwindSafe(|| sup.stop(&mut child, worker))) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            log(format_args!("cannot stop the job cleanly ({e}); waiting for the remaining children"));
            last_resort(&sup, worker);
        }
        Err(_) => last_resort(&sup, worker),
    }
    Err(failed)
}

/// When the orderly stop failed: kill what we can name and reap every child, so the lock
/// outlives the job's processes.
fn last_resort(sup: &Supervisor, worker: Pid) {
    if !sup.worker_reaped.get() {
        let _ = kill_process_group(worker, Signal::KILL);
        let _ = kill_process(worker, Signal::KILL);
    }
    let _ = reap_all();
}

/// `run <job_dir>`: supervises the job with this process's stdout and stderr appended to
/// `<job_dir>/supervisor.log`. Exit code 0 for any terminal job state, 1 when the
/// supervisor itself failed.
pub fn main_run(job_dir: &Path, worker_cmd: &SupervisorCmd) -> i32 {
    // First of all, so that the launcher's group or session dying takes us along only in
    // the shortest possible window (`run_supervisor` repeats it; EPERM is then expected).
    let _ = setsid();
    let job = match JobDir::open(job_dir) {
        Ok(job) => job,
        Err(e) => {
            log(e);
            return 1;
        }
    };
    let redirected = OpenOptions::new()
        .create(true)
        .append(true)
        .open(job.path.join(LOG_FILE))
        .and_then(|f| {
            rustix::stdio::dup2_stdout(&f)?;
            rustix::stdio::dup2_stderr(&f)?;
            Ok(())
        });
    if let Err(e) = redirected {
        log(format_args!("cannot open {LOG_FILE}: {e}"));
        return 1;
    }
    let ran = panic::catch_unwind(|| run_supervisor(&job, worker_cmd));
    match ran.unwrap_or_else(|_| Err(io::Error::other("the supervisor panicked"))) {
        Ok(state) => {
            log(format_args!("job ended {state:?}"));
            0
        }
        Err(e) => {
            log(format_args!("failed: {e}"));
            1
        }
    }
}

/// `worker <job_dir>`: runs the job's effect (`run_worker`) on a current-thread runtime.
/// Its stdio is null, so errors are appended to `supervisor.log`.
pub fn main_worker(job_dir: &Path) -> i32 {
    let ran = JobDir::open(job_dir).and_then(|job| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(run_worker(&job))
    });
    match ran {
        Ok(()) => 0,
        Err(e) => {
            if let Ok(mut f) = OpenOptions::new().append(true).open(job_dir.join(LOG_FILE)) {
                let _ = writeln!(f, "[{}] worker {}: failed: {e}", now_ms(), std::process::id());
            }
            1
        }
    }
}

/// The `agentos-supervisor` command line: `run <job_dir>` or `worker <job_dir>`.
pub fn main_with_args(args: impl IntoIterator<Item = OsString>, worker_cmd: &SupervisorCmd) -> i32 {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [verb, dir] if verb == "run" => main_run(Path::new(dir), worker_cmd),
        [verb, dir] if verb == "worker" => main_worker(Path::new(dir)),
        _ => {
            let _ = writeln!(io::stderr(), "usage: agentos-supervisor run|worker <job_dir>");
            1
        }
    }
}
