//! The code a supervisor's worker process runs: it reads the job's request, runs the
//! effect, and leaves its outcome in the job directory. The supervisor turns that outcome
//! into the receipt; the worker never writes one.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentos_core::ids::{Digest, TaskId};

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use crate::fixture::FixtureExecutor;
use crate::job::{HostConfig, JobDir, WorkerConfig};
use crate::process::{run_in_group, GroupError};

/// The environment variable that must be `1` for a scripted worker to run anything.
pub const TEST_WORKERS_ENV: &str = "AGENTOS_TEST_WORKERS";

const SCRIPT_OUTPUT_LIMIT: usize = 1024 * 1024;
const REASON_STDERR_LIMIT: usize = 4 * 1024;

/// At most `REASON_STDERR_LIMIT` bytes of `stderr` (cut on a char boundary), marked when cut.
fn excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.len() <= REASON_STDERR_LIMIT {
        return text.to_string();
    }
    let mut end = REASON_STDERR_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

pub trait Worker {
    /// Runs one attempt of an effect; every failure is an `Outcome::Failure`.
    fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send;

    /// Whether a dispatched effect without a receipt took effect (see `Executor::reconcile`).
    fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = Reconciliation> + Send;

    /// The current digest of `task`'s workspace (see `Executor::current_workspace`).
    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>>;
}

/// The fixture executor, run inside the worker process.
pub struct HostProcessWorker(FixtureExecutor);

impl HostProcessWorker {
    /// With `groups_file`, every verification's process group is recorded there.
    pub fn new(config: &HostConfig, groups_file: Option<PathBuf>) -> HostProcessWorker {
        let exec = FixtureExecutor::new(
            config.snapshot_dir.clone(),
            config.profile_dir.clone(),
            config.work_root.clone(),
        )
        .with_verify_timeout(Duration::from_secs(config.verify_timeout_secs))
        .with_pinned_profile(config.profile_digest);
        HostProcessWorker(match groups_file {
            Some(file) => exec.inside_worker(file),
            None => exec,
        })
    }
}

impl Worker for HostProcessWorker {
    fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send {
        self.0.run(req, ctx)
    }

    fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = Reconciliation> + Send {
        self.0.reconcile(req, ctx)
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.0.current_workspace(task)
    }
}

/// A test worker: runs `sh -c <script>` and reports its stdout as the success output.
/// It refuses to run unless `AGENTOS_TEST_WORKERS=1` is in its environment.
pub struct ScriptedWorker {
    pub script: String,
    groups_file: Option<PathBuf>,
}

impl ScriptedWorker {
    pub fn new(script: impl Into<String>) -> ScriptedWorker {
        ScriptedWorker { script: script.into(), groups_file: None }
    }

    /// Records the script's process group in `groups_file`.
    pub fn inside_worker(mut self, groups_file: PathBuf) -> ScriptedWorker {
        self.groups_file = Some(groups_file);
        self
    }
}

fn test_workers_enabled() -> bool {
    std::env::var(TEST_WORKERS_ENV).as_deref() == Ok("1")
}

/// The script runs in its own process group with no timeout of its own: the supervisor
/// enforces the lease and the deadline by killing the recorded groups. (See `run_in_group`
/// for the window in which a group exists but is not yet recorded.)
async fn run_scripted(
    script: &str,
    groups_file: Option<&Path>,
    enabled: bool,
    req: &EffectRequest,
    ctx: &AttemptCtx,
) -> ExecOutcome {
    if !enabled {
        return ExecOutcome::failure(req, ctx, "scripted workers are disabled");
    }
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(std::env::temp_dir());
    let output = match run_in_group(cmd, Duration::MAX, SCRIPT_OUTPUT_LIMIT, groups_file).await {
        Ok(o) => o,
        Err(GroupError::Timeout) => return ExecOutcome::failure(req, ctx, "timeout"),
        Err(GroupError::Io(e)) => return ExecOutcome::failure(req, ctx, format!("cannot run script: {e}")),
    };
    if !output.status.success() {
        let stderr = excerpt(&output.stderr);
        return ExecOutcome::failure(req, ctx, format!("script failed ({}): {stderr}", output.status));
    }
    // A cut output is not the output the script produced.
    if output.stdout.len() > SCRIPT_OUTPUT_LIMIT {
        return ExecOutcome::failure(req, ctx, format!("script output exceeds {SCRIPT_OUTPUT_LIMIT} bytes"));
    }
    ExecOutcome::success(req, ctx, output.stdout)
}

impl Worker for ScriptedWorker {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        run_scripted(&self.script, self.groups_file.as_deref(), test_workers_enabled(), req, ctx).await
    }

    async fn reconcile(&self, _req: &EffectRequest, _ctx: &AttemptCtx) -> Reconciliation {
        Reconciliation::Unknown
    }

    fn current_workspace(&self, _task: &TaskId) -> Option<Result<Digest, String>> {
        None
    }
}

/// The worker process's entry point: runs the job's effect and writes `outcome.json` and
/// `outcome.bin`. An effect failure is a failure outcome; `Err` means the job directory
/// itself could not be read or written.
pub async fn run_worker(job: &JobDir) -> io::Result<()> {
    let request = job.request()?;
    // `JobDir::create` checks this too, but a hand-written or older request.json must not
    // resolve against the worker's working directory.
    if let WorkerConfig::Host(h) = &request.worker {
        for (name, p) in [("snapshot_dir", &h.snapshot_dir), ("profile_dir", &h.profile_dir), ("work_root", &h.work_root)] {
            if !p.is_absolute() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name} {} must be absolute", p.display())));
            }
        }
    }
    let req = EffectRequest {
        effect_id: request.effect_id,
        task_id: request.task_id,
        kind: request.kind,
        payload: request.payload,
        contract: request.contract,
    };
    let groups = job.path.join("groups");
    let ctx = |worker: &str| AttemptCtx {
        attempt_id: request.attempt_id.clone(),
        lease_generation: request.lease_generation,
        worker: worker.to_string(),
    };
    let out = match &request.worker {
        WorkerConfig::Host(config) => HostProcessWorker::new(config, Some(groups)).run(&req, &ctx("host-process")).await,
        WorkerConfig::Scripted(config) => {
            ScriptedWorker::new(config.script.clone()).inside_worker(groups).run(&req, &ctx("scripted")).await
        }
    };
    job.write_outcome(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};

    fn req() -> (EffectRequest, AttemptCtx) {
        let json = r#"{"goal":"g","repository":{"source":"s","revision":"r"},"profile":"p","editable_paths":["src/**"],"verification_profile":"v","capabilities":["snapshot.read"],"limits":{"model_requests":1,"max_output_tokens_per_request":1,"tool_actions":1,"deadline_seconds":1,"worker_vcpus":1,"worker_memory_mib":1}}"#;
        let task = TaskId::new();
        let kind = EffectKind::ReadSnapshot;
        let req = EffectRequest {
            effect_id: EffectId::derive(&task, 0, &kind, &Digest::of(b"")),
            task_id: task,
            kind,
            payload: vec![],
            contract: Contract::parse(json).unwrap(),
        };
        (req, AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 2, worker: "scripted".into() })
    }

    #[tokio::test]
    async fn an_enabled_script_reports_its_stdout_and_records_its_own_group() {
        let dir = tempfile::tempdir().unwrap();
        let groups = dir.path().join("groups");
        let (req, ctx) = req();
        let out = run_scripted("echo $$; cut -d' ' -f5 /proc/$$/stat", Some(&groups), true, &req, &ctx).await;
        assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
        assert_eq!(out.receipt.result_digest, Some(Digest::of(&out.output)));
        let stdout = String::from_utf8(out.output).unwrap();
        let recorded = std::fs::read_to_string(&groups).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], lines[1], "the shell leads its own group");
        assert_eq!(recorded, format!("{}\n", lines[0]), "and that group is recorded");
    }

    #[tokio::test]
    async fn a_failing_script_is_a_failure_with_its_stderr() {
        let (req, ctx) = req();
        let out = run_scripted("echo nope >&2; exit 3", None, true, &req, &ctx).await;
        let Outcome::Failure(reason) = out.receipt.outcome else { panic!("expected failure") };
        assert!(reason.contains("nope") && reason.contains('3'), "{reason}");
    }

    #[tokio::test]
    async fn an_oversized_output_is_a_failure_not_a_truncation() {
        let (req, ctx) = req();
        let out = run_scripted("head -c 1048577 /dev/zero", None, true, &req, &ctx).await;
        let Outcome::Failure(reason) = out.receipt.outcome else { panic!("expected failure") };
        assert!(reason.contains("exceeds"), "{reason}");
        let out = run_scripted("head -c 1048576 /dev/zero", None, true, &req, &ctx).await;
        assert_eq!(out.receipt.outcome, Outcome::Success);
        assert_eq!(out.output.len(), SCRIPT_OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn a_failure_reason_keeps_at_most_4_kib_of_stderr() {
        let (req, ctx) = req();
        let out = run_scripted("head -c 100000 /dev/zero | tr '\\0' x >&2; exit 1", None, true, &req, &ctx).await;
        let Outcome::Failure(reason) = out.receipt.outcome else { panic!("expected failure") };
        assert!(reason.starts_with("script failed"), "{reason}");
        assert!(reason.len() <= 4096 + 100, "reason is {} bytes", reason.len());
        assert!(reason.contains(&"x".repeat(4000)) && reason.ends_with("[truncated]"), "{reason}");
    }

    #[tokio::test]
    async fn a_disabled_script_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let (req, ctx) = req();
        let out = run_scripted(&format!("touch {}", marker.display()), None, false, &req, &ctx).await;
        assert_eq!(out.receipt.outcome, Outcome::Failure("scripted workers are disabled".into()));
        assert!(!marker.exists());
    }

    #[test]
    fn an_unrecordable_group_is_killed_and_reported() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let (req, ctx) = req();
        let script = format!("sleep 1; touch {}", marker.display());
        let groups = dir.path().join("missing-dir/groups");
        let out = rt.block_on(run_scripted(&script, Some(&groups), true, &req, &ctx));
        let Outcome::Failure(reason) = out.receipt.outcome else { panic!("expected failure") };
        assert!(reason.contains("cannot record process group"), "{reason}");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the unrecorded script kept running");
    }
}
