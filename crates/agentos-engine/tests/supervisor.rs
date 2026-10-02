mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome};
use agentos_engine::job::{HostConfig, JobDir, JobRequest, JobState, JobStatus, KillReason, ScriptedConfig, WorkerConfig};
use common::{contract, copy_dir, fixtures};
use rustix::process::{kill_process_group, Pid, Signal};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agentos-supervisor");
const TEST_WORKERS: &str = "AGENTOS_TEST_WORKERS";
const EXIT_BEFORE_RECEIPT: &str = "AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT";
const POLL: &str = "AGENTOS_SUPERVISOR_POLL_MS";
const PANIC_AFTER_SPAWN: &str = "AGENTOS_TEST_SUPERVISOR_PANIC_AFTER_SPAWN";
/// Upper bound for anything a test waits on; every test should finish well inside it.
const PATIENCE: Duration = Duration::from_secs(5);

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

struct Fx {
    dir: TempDir,
    task: TaskId,
    contract: Contract,
}

impl Fx {
    fn new() -> Fx {
        Fx { dir: tempfile::tempdir().unwrap(), task: TaskId::new(), contract: contract(10).0 }
    }

    /// With the parser fixture's snapshot and verification profile, for host workers.
    fn with_fixtures() -> Fx {
        let fx = Fx::new();
        copy_dir(&fixtures().join("parser-repo"), &fx.path("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &fx.path("profile"));
        fx
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn host(&self) -> WorkerConfig {
        WorkerConfig::Host(HostConfig {
            snapshot_dir: self.path("snapshot"),
            profile_dir: self.path("profile"),
            work_root: self.path("work"),
            verify_timeout_secs: 60,
            profile_digest: None,
        })
    }

    /// Points the profile at a Python script (on the copy; the fixture is untouched).
    fn script_profile(&self, script: &str) {
        let profile = serde_json::json!({ "id": "pg-test", "command": ["python3", "-c", script], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    /// A request whose lease expires `lease_in_ms` from now, with no task deadline.
    fn request(&self, kind: EffectKind, worker: WorkerConfig, lease_in_ms: i64) -> JobRequest {
        JobRequest {
            effect_id: EffectId::derive(&self.task, 0, &kind, &Digest::of(b"")),
            task_id: self.task.clone(),
            kind,
            payload: Vec::new(),
            contract: self.contract.clone(),
            attempt_id: AttemptId::new(),
            lease_generation: 4,
            lease_expiry_ms: now_ms() + lease_in_ms,
            task_deadline_ms: 0,
            worker,
        }
    }

    fn scripted(&self, kind: EffectKind, script: &str, lease_in_ms: i64) -> JobRequest {
        self.request(kind, WorkerConfig::Scripted(ScriptedConfig { script: script.into() }), lease_in_ms)
    }

    fn create(&self, req: &JobRequest) -> (JobDir, File) {
        JobDir::create(&self.path("jobs"), req).unwrap()
    }
}

/// Where `JobDir::create` will put `req`'s job, for scripts that must name it up front.
fn job_path(fx: &Fx, req: &JobRequest) -> PathBuf {
    fx.path("jobs").join(format!("{}-{}", req.effect_id, req.attempt_id))
}

fn apply_patch() -> EffectKind {
    EffectKind::ApplyPatch { expected_base: Digest::of(b"base") }
}

/// The request and attempt context a worker for `req` runs with.
fn parts(req: &JobRequest) -> (EffectRequest, AttemptCtx) {
    let effect = EffectRequest {
        effect_id: req.effect_id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        payload: req.payload.clone(),
        contract: req.contract.clone(),
    };
    let ctx = AttemptCtx { attempt_id: req.attempt_id.clone(), lease_generation: req.lease_generation, worker: "scripted".into() };
    (effect, ctx)
}

/// Launches the supervisor the way the controller does: the locked `lock` file is its stdin.
fn spawn_with(job: &JobDir, lock: File, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(BIN);
    cmd.arg("run").arg(&job.path).stdin(Stdio::from(lock)).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.env_remove(TEST_WORKERS).env_remove(EXIT_BEFORE_RECEIPT).env_remove(POLL).env_remove(PANIC_AFTER_SPAWN);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn spawn(job: &JobDir, lock: File) -> Child {
    spawn_with(job, lock, &[(TEST_WORKERS, "1")])
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!("supervisor still running after {PATIENCE:?}");
        }
        sleep(Duration::from_millis(5));
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        sleep(Duration::from_millis(2));
    }
}

fn wait_running(job: &JobDir) -> JobStatus {
    wait_for("Running status", || job.read_status().is_some_and(|s| s.state != JobState::Starting));
    let status = job.read_status().unwrap();
    assert_eq!(status.state, JobState::Running, "{status:?}");
    status
}

fn status(job: &JobDir) -> JobStatus {
    job.read_status().expect("a status")
}

fn assert_killed(job: &JobDir, reason: KillReason) -> JobStatus {
    let status = status(job);
    assert_eq!((status.state, status.reason), (JobState::Killed, Some(reason)), "{status:?}");
    status
}

fn failure_reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!("expected a failure, got success: {}", String::from_utf8_lossy(&out.output)),
    }
}

/// The receipt is for this very attempt and carries a failure with `reason`.
fn assert_failure_receipt(job: &JobDir, req: &JobRequest, reason: &str) {
    let receipt = job.read_receipt().expect("a receipt");
    assert_eq!(failure_reason(&receipt), reason);
    assert_eq!(receipt.receipt.effect_id, req.effect_id);
    assert_eq!(receipt.receipt.attempt_id, req.attempt_id);
    assert_eq!(receipt.receipt.lease_generation, req.lease_generation);
}

/// A process is gone when `/proc` no longer has it or it is a zombie (nobody reaps orphans
/// in the test container).
fn gone(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_whitespace().next() == Some("Z")
}

fn pids(file: &Path) -> Vec<i32> {
    fs::read_to_string(file).unwrap().split_whitespace().map(|p| p.parse().unwrap()).collect()
}

fn assert_all_gone(pids: &[i32]) {
    let live: Vec<i32> = pids.iter().copied().filter(|p| !gone(*p)).collect();
    assert!(live.is_empty(), "still running: {live:?} of {pids:?}");
}

#[test]
fn normal_exit_writes_receipt_before_terminal_status() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "sleep 0.3; echo hi", 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    let mut seen = Vec::new();
    let started = Instant::now();
    loop {
        if let Some(s) = job.read_status() {
            seen.push(s.state);
            if s.state == JobState::Exited {
                assert!(job.read_receipt().is_some(), "the status became terminal before the receipt");
                break;
            }
            assert_ne!(s.state, JobState::Killed, "{s:?}");
        }
        assert!(started.elapsed() < PATIENCE, "no terminal status");
        sleep(Duration::from_millis(1));
    }
    assert!(wait_exit(&mut child).success());
    assert!(seen.contains(&JobState::Running), "{seen:?}");

    let receipt = job.read_receipt().unwrap();
    assert_eq!(receipt.receipt.outcome, Outcome::Success);
    assert_eq!(receipt.output, b"hi\n");
    assert_eq!(receipt.receipt.effect_id, req.effect_id);
    assert_eq!(receipt.receipt.attempt_id, req.attempt_id);
    assert_eq!(receipt.receipt.lease_generation, req.lease_generation);
    assert_eq!(Some(receipt), job.read_outcome(), "the receipt is the worker's outcome");
    let status = status(&job);
    assert_eq!(status.reason, None);
    assert_eq!(status.supervisor_pid, Some(child.id()));
    assert!(status.worker_pgid.is_some());
    assert!(!job.lock_held());
}

#[test]
fn lease_expiry_kills_a_forked_grandchild_and_its_files_never_appear() {
    let fx = Fx::new();
    let (p, m) = (fx.path("pids"), fx.path("marker"));
    let mut req = fx.scripted(EffectKind::ReadSnapshot, "", 1000);
    let groups = job_path(&fx, &req).join("groups");
    // If the subshell survived the kill, the marker would appear about 1.5 s after the start.
    // The Python child leads a group of its own, which the script records like a check would.
    req.worker = WorkerConfig::Scripted(ScriptedConfig {
        script: format!(
            "echo $$ >> {p}; (sleep 1.5 & echo $! >> {p}; wait; touch {m}) & echo $! >> {p}; \
             python3 -c 'import os, time; os.setpgid(0, 0); time.sleep(30)' & echo $! >> {p}; echo $! >> {g}; \
             sleep 30 & echo $! >> {p}; wait",
            p = p.display(),
            m = m.display(),
            g = groups.display(),
        ),
    });
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());

    let status = assert_killed(&job, KillReason::Lease);
    assert_failure_receipt(&job, &req, "lease expired");
    assert!(job.read_outcome().is_none(), "the worker was killed before it wrote an outcome");
    let mut recorded = pids(&fx.path("pids"));
    assert_eq!(recorded.len(), 5, "the script did not start all its processes: {recorded:?}");
    assert_eq!(job.groups().len(), 2, "the script's group and the Python child's group");
    recorded.push(status.worker_pgid.unwrap());
    assert_all_gone(&recorded);
    sleep(Duration::from_secs(2));
    assert!(!fx.path("marker").exists(), "a forked grandchild survived the lease");
    assert_all_gone(&recorded);
}

#[test]
fn a_verification_check_group_is_killed_with_the_worker() {
    let fx = Fx::with_fixtures();
    let snap = fx.request(EffectKind::ReadSnapshot, fx.host(), 4000);
    let (job, lock) = fx.create(&snap);
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());
    assert_eq!(status(&job).state, JobState::Exited);
    assert_eq!(job.read_receipt().unwrap().receipt.outcome, Outcome::Success);

    let (p, m) = (fx.path("pids"), fx.path("marker"));
    fx.script_profile(&format!(
        "import os, subprocess, time\n\
         child = subprocess.Popen(['sh', '-c', 'sleep 2.5; touch {m}'])\n\
         with open({p:?}, 'w') as f: f.write(f'{{os.getpid()}} {{child.pid}}')\n\
         time.sleep(30)\n",
        m = m.display(),
        p = p.to_str().unwrap(),
    ));
    // If the child survived, the marker would appear about 1 s after the lease expired.
    let verify = fx.request(EffectKind::RunVerification, fx.host(), 1500);
    let (job, lock) = fx.create(&verify);
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());

    assert_killed(&job, KillReason::Lease);
    assert_failure_receipt(&job, &verify, "lease expired");
    assert!(p.exists(), "the check never started, so the lease did not interrupt it");
    let check = pids(&p);
    assert_eq!(job.groups(), vec![check[0]], "the check's own group is recorded");
    assert_all_gone(&check);
    sleep(Duration::from_secs(2));
    assert!(!m.exists(), "the check's background child survived the lease");
}

#[test]
fn killed_apply_patch_writes_no_receipt_only_killed_status() {
    for kind in [apply_patch(), EffectKind::ExportBundle] {
        let fx = Fx::new();
        let req = fx.scripted(kind.clone(), "sleep 30", 300);
        let (job, lock) = fx.create(&req);
        let mut child = spawn(&job, lock);
        assert!(wait_exit(&mut child).success());

        assert_killed(&job, KillReason::Lease);
        assert!(job.read_receipt().is_none(), "{kind:?}");
        assert!(!job.path.join("receipt.json").exists() && !job.path.join("output.bin").exists(), "{kind:?}");
        assert!(job.is_dead());
    }
}

#[test]
fn kill_publishes_a_valid_outcome_json_as_the_receipt() {
    let fx = Fx::new();
    let req = fx.scripted(apply_patch(), "sleep 30", 1000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    wait_running(&job);
    // The work finished and the worker then hung before exiting.
    let (effect, ctx) = parts(&req);
    let out = ExecOutcome::success(&effect, &ctx, b"applied".to_vec());
    job.write_outcome(&out).unwrap();
    assert!(wait_exit(&mut child).success());

    assert_killed(&job, KillReason::Lease);
    assert_eq!(job.read_receipt(), Some(out));
}

#[test]
fn deadline_beats_lease_in_the_reason() {
    let fx = Fx::new();
    let mut req = fx.scripted(EffectKind::ReadSnapshot, "sleep 30", 500);
    req.task_deadline_ms = req.lease_expiry_ms;
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());

    assert_killed(&job, KillReason::Deadline);
    assert_failure_receipt(&job, &req, "deadline exceeded");
}

#[test]
fn cancel_wins_over_both() {
    let fx = Fx::new();
    let mut req = fx.scripted(EffectKind::ReadSnapshot, "sleep 30", 1000);
    req.task_deadline_ms = req.lease_expiry_ms;
    let (job, lock) = fx.create(&req);
    // One check right after the spawn, the next one long after both have expired.
    let mut child = spawn_with(&job, lock, &[(TEST_WORKERS, "1"), (POLL, "2500")]);
    wait_running(&job);
    assert!(now_ms() < req.lease_expiry_ms, "the supervisor started too slowly for this test");
    while now_ms() <= req.lease_expiry_ms + 100 {
        sleep(Duration::from_millis(10));
    }
    job.drop_cancel().unwrap();
    assert!(wait_exit(&mut child).success());

    assert_killed(&job, KillReason::Cancel);
    assert_failure_receipt(&job, &req, "cancelled");
}

#[test]
fn cancel_marker_present_before_start_spawns_no_worker() {
    for kind in [EffectKind::ReadSnapshot, apply_patch()] {
        let fx = Fx::new();
        let marker = fx.path("marker");
        let req = fx.scripted(kind.clone(), &format!("touch {}", marker.display()), 5000);
        let (job, lock) = fx.create(&req);
        job.drop_cancel().unwrap();
        let mut child = spawn(&job, lock);
        assert!(wait_exit(&mut child).success());

        let status = assert_killed(&job, KillReason::Cancel);
        assert_eq!(status.worker_pgid, None, "a worker was spawned");
        if kind == EffectKind::ReadSnapshot {
            assert_failure_receipt(&job, &req, "cancelled");
        } else {
            assert!(job.read_receipt().is_none());
        }
        sleep(Duration::from_millis(300));
        assert!(!marker.exists(), "the worker ran");
        assert!(job.read_outcome().is_none() && !job.path.join("groups").exists());
    }
}

#[test]
fn cancel_marker_kills_within_500ms() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "sleep 30", 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    let running = wait_running(&job);
    let dropped = Instant::now();
    job.drop_cancel().unwrap();
    wait_for("terminal status", || job.read_status().is_some_and(|s| s.state == JobState::Killed));
    let took = dropped.elapsed();
    assert!(took < Duration::from_millis(500), "took {took:?}");
    assert!(wait_exit(&mut child).success());

    assert_killed(&job, KillReason::Cancel);
    assert_failure_receipt(&job, &req, "cancelled");
    assert!(gone(running.worker_pgid.unwrap()));
}

#[test]
fn normal_exit_reaps_a_background_child() {
    let fx = Fx::new();
    let (p, m) = (fx.path("pid"), fx.path("marker"));
    // `setsid` takes the child out of every recorded group: only the supervisor, as the
    // subreaper it is reparented to, can still stop it.
    let script = format!(
        "setsid sh -c 'echo $$ > {p}; sleep 1; touch {m}' </dev/null >/dev/null 2>&1 &\n\
         while [ ! -s {p} ]; do sleep 0.01; done; echo ok",
        p = p.display(),
        m = m.display(),
    );
    let req = fx.scripted(EffectKind::ReadSnapshot, &script, 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());

    assert_eq!(status(&job).state, JobState::Exited);
    let receipt = job.read_receipt().unwrap();
    assert_eq!((receipt.receipt.outcome, receipt.output), (Outcome::Success, b"ok\n".to_vec()));
    let background = pids(&p);
    assert_all_gone(&background);
    sleep(Duration::from_millis(1500));
    assert!(!m.exists(), "the background child outlived the job");
}

#[test]
fn invalid_outcome_from_the_worker_becomes_a_failure_receipt() {
    // Each case leaves a forged (or no) outcome.json behind and SIGKILLs the worker, so the
    // worker never overwrites it with its real outcome.
    type Forge = fn(&mut EffectRequest, &mut AttemptCtx);
    let cases: [(&str, Option<Forge>); 5] = [
        ("forged effect id", Some(|e, _| e.effect_id = EffectId::derive(&TaskId::new(), 0, &e.kind, &Digest::of(b"")))),
        ("other attempt", Some(|_, c| c.attempt_id = AttemptId::new())),
        ("other lease generation", Some(|_, c| c.lease_generation += 1)),
        ("torn output", Some(|_, _| ())),
        ("no outcome", None),
    ];
    for (kind, (case, forge)) in [EffectKind::ReadSnapshot, apply_patch(), EffectKind::ExportBundle]
        .into_iter()
        .flat_map(|kind| cases.iter().map(move |c| (kind.clone(), *c)))
    {
        let fx = Fx::new();
        let staged = fx.path("staged");
        fs::create_dir(&staged).unwrap();
        let req = fx.scripted(kind.clone(), "", 5000);
        if let Some(forge) = forge {
            let (mut effect, mut ctx) = parts(&req);
            forge(&mut effect, &mut ctx);
            JobDir::open(&staged).unwrap().write_outcome(&ExecOutcome::success(&effect, &ctx, b"forged".to_vec())).unwrap();
            if case == "torn output" {
                fs::write(staged.join("outcome.bin"), b"not what the digest says").unwrap();
            }
        }
        let mut req = req;
        let job_path = job_path(&fx, &req);
        req.worker = WorkerConfig::Scripted(ScriptedConfig {
            script: format!(
                "cp {s}/outcome.bin {s}/outcome.json {j}/ 2>/dev/null; kill -9 $PPID; sleep 30",
                s = staged.display(),
                j = job_path.display()
            ),
        });
        let (job, lock) = fx.create(&req);
        assert_eq!(job.path, job_path);
        let mut child = spawn(&job, lock);
        assert!(wait_exit(&mut child).success(), "{kind:?} {case}");

        assert_eq!(status(&job).state, JobState::Exited, "{kind:?} {case}");
        if kind == EffectKind::ReadSnapshot {
            assert_failure_receipt(&job, &req, "worker produced an invalid outcome");
        } else {
            // The effect may or may not have happened: the controller reconciles it.
            assert!(job.read_receipt().is_none(), "{kind:?} {case}");
            assert!(!job.path.join("receipt.json").exists() && !job.path.join("output.bin").exists(), "{kind:?} {case}");
            assert!(job.is_dead(), "{kind:?} {case}");
        }
    }
}

#[test]
fn supervisor_survives_the_launcher_dying() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "sleep 0.5; echo done", 5000);
    let (job, lock) = fx.create(&req);
    // The launcher leads its own session and group, hands its stdin (the lock) to a
    // background supervisor, then lingers without the lock until its whole group is
    // SIGKILLed: a supervisor that stayed in the launcher's group would die with it.
    let mut launcher = Command::new("setsid")
        .arg("sh")
        .arg("-c")
        .arg("exec 3<&0; \"$0\" run \"$1\" 0<&3 3<&- >/dev/null 2>&1 & echo $!; exec sleep 30 0</dev/null 3<&-")
        .arg(BIN)
        .arg(&job.path)
        .env(TEST_WORKERS, "1")
        .stdin(Stdio::from(lock))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(launcher.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let supervisor: u32 = line.trim().parse().unwrap();
    // The supervisor leaves the launcher's session before it writes any status.
    wait_for("the supervisor to start", || job.read_status().is_some());
    let launcher_pid = Pid::from_raw(launcher.id() as i32).unwrap();
    assert_eq!(rustix::process::getsid(Some(launcher_pid)).unwrap(), launcher_pid, "the launcher leads its session");
    kill_process_group(launcher_pid, Signal::KILL).unwrap();
    launcher.wait().unwrap();
    assert!(job.lock_held(), "the supervisor died with its launcher");

    wait_for("Exited", || job.read_status().is_some_and(|s| s.state == JobState::Exited));
    let status = status(&job);
    assert_eq!(status.supervisor_pid, Some(supervisor));
    let receipt = job.read_receipt().unwrap();
    assert_eq!((receipt.receipt.outcome, receipt.output), (Outcome::Success, b"done\n".to_vec()));
    wait_for("the lock to be released", || !job.lock_held());
}

#[test]
fn lock_is_held_for_the_whole_life_of_the_supervisor_and_free_after_exit() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "sleep 0.5", 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    let (mut probes, mut worker_stdio_checked) = (0, false);
    while child.try_wait().unwrap().is_none() {
        if !job.lock_held() {
            // Only an exiting supervisor may have dropped it: after its terminal status,
            // and on its way out.
            assert_eq!(status(&job).state, JobState::Exited, "the lock was free while the supervisor lived");
            wait_exit(&mut child);
            break;
        }
        probes += 1;
        if !worker_stdio_checked && let Some(pgid) = job.read_status().and_then(|s| s.worker_pgid) {
            // The worker leads its group, so this is its pid; it must not hold the lock.
            let fds: Vec<_> = (0..3).map(|fd| fs::read_link(format!("/proc/{pgid}/fd/{fd}"))).collect();
            if fds.iter().all(|l| l.is_ok()) {
                for l in fds {
                    assert_eq!(l.unwrap(), Path::new("/dev/null"));
                }
                worker_stdio_checked = true;
            }
        }
        sleep(Duration::from_millis(5));
    }
    assert!(probes > 10, "only {probes} probes");
    assert!(worker_stdio_checked, "never saw the worker's descriptors");
    assert_eq!(status(&job).state, JobState::Exited);
    assert!(!job.lock_held());
}

#[test]
fn supervisor_sigkill_leaves_running_status_no_receipt_and_a_free_lock() {
    let fx = Fx::new();
    let p = fx.path("pid");
    let req = fx.scripted(EffectKind::ReadSnapshot, &format!("echo $$ > {}; sleep 30", p.display()), 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    let running = wait_running(&job);
    wait_for("the script", || p.exists());
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(status(&job), running, "the status moved on without the supervisor");
    assert!(job.read_receipt().is_none() && !job.path.join("receipt.json").exists());
    assert!(!job.lock_held());
    assert!(job.is_dead());
    // The orphaned worker is the controller's fence to kill (Task 7); clean it up here.
    for pgid in job.groups().into_iter().chain(running.worker_pgid) {
        let _ = kill_process_group(Pid::from_raw(pgid).unwrap(), Signal::KILL);
    }
}

#[test]
fn exit_before_receipt_hook_leaves_outcome_but_no_receipt() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "echo hi", 5000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn_with(&job, lock, &[(TEST_WORKERS, "1"), (EXIT_BEFORE_RECEIPT, "1")]);
    assert_eq!(wait_exit(&mut child).code(), Some(3));

    let outcome = job.read_outcome().expect("the worker's outcome");
    assert_eq!((outcome.receipt.outcome, outcome.output), (Outcome::Success, b"hi\n".to_vec()));
    assert!(job.read_receipt().is_none() && !job.path.join("receipt.json").exists());
    assert_eq!(status(&job).state, JobState::Running);
    assert!(!job.lock_held());

    // Without the test-workers guard the hook is inert.
    let fx = Fx::with_fixtures();
    let req = fx.request(EffectKind::ReadSnapshot, fx.host(), 4000);
    let (job, lock) = fx.create(&req);
    let mut child = spawn_with(&job, lock, &[(EXIT_BEFORE_RECEIPT, "1")]);
    assert_eq!(wait_exit(&mut child).code(), Some(0));
    assert_eq!(status(&job).state, JobState::Exited);
    assert_eq!(job.read_receipt().unwrap().receipt.outcome, Outcome::Success);
}

#[test]
fn an_unreadable_request_exits_1_and_is_logged() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "echo hi", 5000);
    let (job, lock) = fx.create(&req);
    fs::write(job.path.join("request.json"), b"{not json").unwrap();
    let mut child = spawn(&job, lock);
    assert_eq!(wait_exit(&mut child).code(), Some(1));

    let log = fs::read_to_string(job.path.join("supervisor.log")).unwrap();
    assert!(log.contains("request"), "{log}");
    assert!(job.read_receipt().is_none());
    assert!(!job.lock_held());
}

#[test]
fn lease_kill_writes_receipt_before_killed_status_and_holds_the_lock_until_exit() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "sleep 30", 500);
    let (job, lock) = fx.create(&req);
    let mut child = spawn(&job, lock);
    let (mut saw_killed, started) = (false, Instant::now());
    loop {
        let held = job.lock_held();
        let status = job.read_status();
        if let Some(s) = &status
            && s.state == JobState::Killed
        {
            assert!(job.read_receipt().is_some(), "the status became Killed before the receipt");
            saw_killed = true;
        }
        if !held {
            // Only an exiting supervisor may have dropped it, after its terminal status.
            assert_eq!(job.read_status().map(|s| s.state), Some(JobState::Killed), "the lock was free while the job ran");
            break;
        }
        assert!(started.elapsed() < PATIENCE, "the supervisor never released the lock");
        sleep(Duration::from_millis(1));
    }
    assert!(wait_exit(&mut child).success());
    assert!(saw_killed || job.read_status().is_some());
    assert_killed(&job, KillReason::Lease);
    assert_failure_receipt(&job, &req, "lease expired");
}

#[test]
fn a_recorded_group_in_another_session_is_never_killed() {
    let fx = Fx::new();
    let req = fx.scripted(EffectKind::ReadSnapshot, "echo hi", 5000);
    let (job, lock) = fx.create(&req);
    // A forged `groups` file: a group in a session of its own, and one in the test's
    // session (where the controller would live).
    let mut own_session = Command::new("setsid").arg("sleep").arg("30").spawn().unwrap();
    let mut test_session = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    wait_for("the helper's session", || {
        let pid = Pid::from_raw(own_session.id() as i32).unwrap();
        rustix::process::getsid(Some(pid)).is_ok_and(|sid| sid == pid)
    });
    job.record_group(own_session.id() as i32).unwrap();
    job.record_group(test_session.id() as i32).unwrap();
    let mut child = spawn(&job, lock);
    assert!(wait_exit(&mut child).success());

    assert_eq!(status(&job).state, JobState::Exited);
    assert_eq!(job.read_receipt().unwrap().output, b"hi\n");
    sleep(Duration::from_millis(100));
    let survived = [own_session.try_wait().unwrap(), test_session.try_wait().unwrap()];
    for helper in [&mut own_session, &mut test_session] {
        let _ = helper.kill();
        let _ = helper.wait();
    }
    assert_eq!(survived, [None, None], "a group outside the supervisor's session was killed");
    let log = fs::read_to_string(job.path.join("supervisor.log")).unwrap();
    assert_eq!(log.matches("not in our session").count(), 2, "{log}");
}

#[test]
fn a_panic_after_the_spawn_kills_the_job_before_the_lock_is_released() {
    let fx = Fx::new();
    let (p, m) = (fx.path("pids"), fx.path("marker"));
    let mut req = fx.scripted(EffectKind::ReadSnapshot, "", 5000);
    let trigger = job_path(&fx, &req).join("panic-now");
    req.worker = WorkerConfig::Scripted(ScriptedConfig {
        script: format!(
            "echo $$ >> {p}; (sleep 1 & echo $! >> {p}; wait; touch {m}) & echo $! >> {p}; touch {t}; sleep 30",
            p = p.display(),
            m = m.display(),
            t = trigger.display(),
        ),
    });
    let (job, lock) = fx.create(&req);
    let mut child = spawn_with(&job, lock, &[(TEST_WORKERS, "1"), (PANIC_AFTER_SPAWN, "1")]);
    let started = Instant::now();
    loop {
        if !job.lock_held() {
            // Everything of the job must already be dead when the lock frees.
            let pids = if p.exists() { pids(&p) } else { Vec::new() };
            assert_all_gone(&pids);
            if let Some(pgid) = job.read_status().and_then(|s| s.worker_pgid) {
                assert!(gone(pgid), "the worker outlived the lock");
            }
            break;
        }
        assert!(started.elapsed() < PATIENCE, "the supervisor never released the lock");
        sleep(Duration::from_millis(1));
    }
    assert_eq!(wait_exit(&mut child).code(), Some(1));

    let log = fs::read_to_string(job.path.join("supervisor.log")).unwrap();
    assert!(log.contains("panicked"), "{log}");
    assert_eq!(status(&job).state, JobState::Running, "no terminal status after a panic");
    assert!(job.read_receipt().is_none());
    assert_eq!(pids(&p).len(), 3, "the script did not start before the panic");
    assert!(trigger.exists());
    sleep(Duration::from_millis(1500));
    assert!(!m.exists(), "a process of the job outlived the panicking supervisor");
}
