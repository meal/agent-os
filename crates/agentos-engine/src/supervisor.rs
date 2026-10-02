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
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::RetryPolicy;
use rustix::io::Errno;
use rustix::process::{
    getpgrp, getpid, kill_process, kill_process_group, set_child_subreaper, setsid, wait, waitid, Pid, Signal,
    WaitId, WaitIdOptions, WaitOptions,
};

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome};
use crate::job::{JobDir, JobRequest, JobState, JobStatus, KillReason};
use crate::worker::{run_worker, TEST_WORKERS_ENV};

/// Overrides the 50 ms poll interval (tests only).
pub const POLL_ENV: &str = "AGENTOS_SUPERVISOR_POLL_MS";
/// Test-only hook, honoured only together with `AGENTOS_TEST_WORKERS=1`: exit with
/// `EXIT_BEFORE_RECEIPT_CODE` right after reading a valid `outcome.json`, before the
/// receipt is written (the Phase 2 "executed but not durable" case).
pub const EXIT_BEFORE_RECEIPT_ENV: &str = "AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT";
pub const EXIT_BEFORE_RECEIPT_CODE: i32 = 3;
pub const LOG_FILE: &str = "supervisor.log";

const DEFAULT_POLL: Duration = Duration::from_millis(50);
/// How long to let SIGKILLed children die before looking for them again.
const REAP_PAUSE: Duration = Duration::from_millis(5);

/// How the supervisor starts its worker: `program prefix_args… worker <job_dir>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorCmd {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn log(msg: impl std::fmt::Display) {
    eprintln!("[{}] supervisor {}: {msg}", now_ms(), std::process::id());
}

fn poll_interval() -> Duration {
    std::env::var(POLL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_POLL, Duration::from_millis)
}

fn exit_before_receipt_hook() -> bool {
    let on = |var| std::env::var(var).as_deref() == Ok("1");
    on(TEST_WORKERS_ENV) && on(EXIT_BEFORE_RECEIPT_ENV)
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
    };
    let ctx = AttemptCtx {
        attempt_id: req.attempt_id.clone(),
        lease_generation: req.lease_generation,
        worker: "supervisor".into(),
    };
    ExecOutcome::failure(&effect, &ctx, reason)
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

/// SIGKILLs group `pgid`, never init's (1, which would mean "everyone") nor our own.
fn kill_group(pgid: i32) {
    if pgid > 1
        && pgid != getpgrp().as_raw_nonzero().get()
        && let Some(p) = Pid::from_raw(pgid)
    {
        let _ = kill_process_group(p, Signal::KILL);
    }
}

/// SIGKILLs every live child of this process. A child's pid cannot be reused before we
/// reap it, so nothing else can be hit.
fn kill_children() -> io::Result<()> {
    let me = std::process::id().to_string();
    for entry in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        // Fields after the command name, which may itself contain spaces and parentheses.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { continue };
        let mut fields = rest.split_whitespace();
        let (state, ppid) = (fields.next(), fields.next());
        if ppid == Some(me.as_str())
            && state != Some("Z")
            && let Some(p) = Pid::from_raw(pid)
        {
            let _ = kill_process(p, Signal::KILL);
        }
    }
    Ok(())
}

/// Reaps children until none is left, killing any that still live.
fn reap_all() -> io::Result<()> {
    loop {
        match wait(WaitOptions::NOHANG) {
            Ok(Some(_)) | Err(Errno::INTR) => {}
            Ok(None) => {
                kill_children()?;
                std::thread::sleep(REAP_PAUSE);
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
            // The worker is not reaped yet, so its group id still names only its group.
            let _ = kill_process_group(worker, Signal::KILL);
            child.wait()?;
            self.worker_reaped.set(true);
        }
        // Read only now: the worker can no longer record a group.
        for pgid in self.job.groups() {
            kill_group(pgid);
        }
        // Catches what left every recorded group (setsid, or a group the worker had no
        // time to record): orphans are reparented to us.
        reap_all()
    }

    /// The KILL-RECEIPT RULE: a valid outcome is published; otherwise only effects that
    /// leave no lasting change get a failure receipt; then the terminal status.
    fn finish_killed(&self, reason: KillReason, worker_pgid: Option<i32>) -> io::Result<JobState> {
        log(format_args!("killing the job: {}", kill_text(reason)));
        let receipt = valid_outcome(self.job, &self.req).or_else(|| match self.req.kind.retry_policy() {
            RetryPolicy::Retry => Some(failure(&self.req, kill_text(reason))),
            // The effect may or may not have happened: only reconciliation can tell.
            RetryPolicy::ReconcileThenRetry | RetryPolicy::NoRetry => None,
        });
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
        if outcome.is_some() && exit_before_receipt_hook() {
            log("test hook: exiting before the receipt");
            std::process::exit(EXIT_BEFORE_RECEIPT_CODE);
        }
        self.stop(child, worker)?;
        let receipt = outcome.unwrap_or_else(|| {
            log("the worker exited without a valid outcome");
            failure(&self.req, "worker produced an invalid outcome")
        });
        self.job.write_receipt(&receipt)?;
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
    // Already a group leader (EPERM) is fine: we only need to leave the launcher's session.
    match setsid() {
        Ok(_) | Err(Errno::PERM) => {}
        Err(e) => return Err(e.into()),
    }
    set_child_subreaper(Some(getpid()))?;
    if !job.lock_held() {
        return Err(io::Error::other("the job lock is not held: launch the supervisor with the locked lock file as stdin"));
    }
    let sup = Supervisor { job, req, pid: std::process::id(), worker_reaped: Cell::new(false) };
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
    log(format_args!("worker {worker:?} started"));
    sup.supervise(&mut child, worker, poll_interval()).inspect_err(|_| {
        // The lock is released when we exit; nothing of the job may outlive it.
        if let Err(e) = sup.stop(&mut child, worker) {
            log(format_args!("cannot stop the worker: {e}"));
        }
    })
}

/// `run <job_dir>`: supervises the job with this process's stdout and stderr appended to
/// `<job_dir>/supervisor.log`. Exit code 0 for any terminal job state, 1 when the
/// supervisor itself failed.
pub fn main_run(job_dir: &Path, worker_cmd: &SupervisorCmd) -> i32 {
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
    match run_supervisor(&job, worker_cmd) {
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
            eprintln!("usage: agentos-supervisor run|worker <job_dir>");
            1
        }
    }
}
