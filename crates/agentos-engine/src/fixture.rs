//! Local-directory executor: a per-task copy of a fixture snapshot, patched with `git apply`
//! and checked by a protected profile that lives outside the workspace.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use agentos_core::effect::EffectKind;
use agentos_core::ids::{Digest, TaskId};
use serde::Deserialize;
use serde_json::json;

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation, VerificationReport};
use crate::patch::{git, paths_of_file};
use crate::process::{run_in_group, GroupError};
use crate::workspace::{copy_tree, excluded_entries, has_excluded_component, purge_excluded, workspace_digest};

const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Deserialize)]
struct Profile {
    id: String,
    command: Vec<String>,
}

pub struct FixtureExecutor {
    snapshot_dir: PathBuf,
    profile_dir: PathBuf,
    work_root: PathBuf,
    verify_timeout: Duration,
    /// Where each verification's process group is recorded; set inside a worker.
    groups_file: Option<PathBuf>,
    /// The digest the staged profile must have before anything of it runs.
    pinned_profile: Option<Digest>,
}

/// Runs blocking filesystem work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(io::Error::other)?
}

fn truncated(bytes: &[u8]) -> (String, bool) {
    let cut = bytes.len() > OUTPUT_LIMIT;
    (String::from_utf8_lossy(&bytes[..bytes.len().min(OUTPUT_LIMIT)]).into_owned(), cut)
}

/// The first prefix of `rel` (inside `ws`) that is a symlink, if any.
fn symlink_on_path(ws: &Path, rel: &str) -> io::Result<Option<String>> {
    let mut cur = ws.to_path_buf();
    for comp in Path::new(rel).components() {
        let Component::Normal(name) = comp else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unexpected component in {rel}")));
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Ok(Some(cur.strip_prefix(ws).unwrap_or(&cur).display().to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

impl FixtureExecutor {
    pub fn new(snapshot_dir: PathBuf, profile_dir: PathBuf, work_root: PathBuf) -> FixtureExecutor {
        FixtureExecutor {
            snapshot_dir,
            profile_dir,
            work_root,
            verify_timeout: Duration::from_secs(60),
            groups_file: None,
            pinned_profile: None,
        }
    }

    /// Records every verification's process group in `groups_file`, so the supervisor can
    /// kill it; the check still runs in a group of its own.
    pub fn inside_worker(mut self, groups_file: PathBuf) -> FixtureExecutor {
        self.groups_file = Some(groups_file);
        self
    }

    /// Refuses to run a verification whose staged profile does not have digest `pinned`.
    pub fn with_pinned_profile(mut self, pinned: Option<Digest>) -> FixtureExecutor {
        self.pinned_profile = pinned;
        self
    }

    pub fn with_verify_timeout(mut self, timeout: Duration) -> FixtureExecutor {
        self.verify_timeout = timeout;
        self
    }

    fn task_dir(&self, task: &TaskId) -> PathBuf {
        self.work_root.join(task.as_str())
    }

    pub fn workspace(&self, task: &TaskId) -> PathBuf {
        self.task_dir(task).join("ws")
    }

    async fn read_snapshot(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        let (from, ws) = (self.snapshot_dir.clone(), self.workspace(&req.task_id));
        let copied = blocking(move || {
            // A retry starts from a clean copy, so the effect is idempotent.
            if ws.exists() {
                std::fs::remove_dir_all(&ws)?;
            }
            let files = copy_tree(&from, &ws)?;
            Ok((files, workspace_digest(&ws)?))
        })
        .await;
        match copied {
            Ok((files, digest)) => {
                let output = json!({ "files": files, "workspace_digest": digest });
                let mut out = ExecOutcome::success(req, ctx, output.to_string().into_bytes());
                out.new_workspace = Some(digest);
                out
            }
            Err(e) => ExecOutcome::failure(req, ctx, format!("snapshot failed: {e}")),
        }
    }

    /// The success outcome of an applied patch; reconciliation rebuilds the same bytes.
    fn patch_applied(req: &EffectRequest, ctx: &AttemptCtx, paths: Vec<String>, digest: Digest) -> ExecOutcome {
        let output = json!({ "applied": true, "paths": paths, "workspace_digest": digest });
        let mut out = ExecOutcome::success(req, ctx, output.to_string().into_bytes());
        out.new_workspace = Some(digest);
        out
    }

    async fn apply_patch(&self, req: &EffectRequest, ctx: &AttemptCtx, expected_base: Digest) -> ExecOutcome {
        match self.try_apply_patch(req, expected_base).await {
            Ok((paths, digest)) => FixtureExecutor::patch_applied(req, ctx, paths, digest),
            Err(reason) => ExecOutcome::failure(req, ctx, reason),
        }
    }

    /// Whether the patch in `req` was applied to the workspace: `Ok(None)` if the workspace
    /// is still exactly the expected base, `Ok(Some(..))` if it is exactly the base plus
    /// this patch (reverting the patch on a scratch copy gives the base back), else `Err`.
    async fn patch_state(&self, req: &EffectRequest, expected_base: Digest) -> Result<Option<(Vec<String>, Digest)>, String> {
        let ws = self.workspace(&req.task_id);
        if !ws.is_dir() {
            return Err("workspace missing".into());
        }
        let actual = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;
        if actual == expected_base {
            return Ok(None);
        }
        let scratch =
            tempfile::tempdir_in(self.task_dir(&req.task_id)).map_err(|e| format!("scratch dir: {e}"))?;
        let patch_file = scratch.path().join("change.patch");
        std::fs::write(&patch_file, &req.payload).map_err(|e| format!("cannot write patch: {e}"))?;
        let paths = paths_of_file(&patch_file, scratch.path()).await?;
        let copy = scratch.path().join("ws");
        copy_tree(&ws, &copy).map_err(|e| format!("cannot copy workspace: {e}"))?;
        let reverted = git(&copy)
            .args(["apply", "--reverse"])
            .arg(&patch_file)
            .output()
            .await
            .map_err(|e| format!("cannot run git: {e}"))?;
        let reverted = reverted.status.success() && workspace_digest(&copy).is_ok_and(|d| d == expected_base);
        if !reverted {
            return Err(format!("workspace {actual} is neither the base {expected_base} nor the base with this patch"));
        }
        Ok(Some((paths, actual)))
    }

    /// Validation order: parse, editable paths, symlinks, expected base; only then git.
    async fn try_apply_patch(
        &self,
        req: &EffectRequest,
        expected_base: Digest,
    ) -> Result<(Vec<String>, Digest), String> {
        let ws = self.workspace(&req.task_id);
        if !ws.is_dir() {
            return Err("workspace missing: no snapshot was read".into());
        }
        // The patch file lives next to, never inside, the workspace.
        let scratch =
            tempfile::tempdir_in(self.task_dir(&req.task_id)).map_err(|e| format!("scratch dir: {e}"))?;
        let patch_file = scratch.path().join("change.patch");
        std::fs::write(&patch_file, &req.payload).map_err(|e| format!("cannot write patch: {e}"))?;

        let paths = paths_of_file(&patch_file, scratch.path()).await?;
        if let Some(p) = paths.iter().find(|p| !req.contract.path_allowed(p)) {
            return Err(format!("path not editable: {p}"));
        }
        if let Some(p) = paths.iter().find(|p| has_excluded_component(p)) {
            return Err(format!("path excluded from the workspace digest: {p}"));
        }
        for p in &paths {
            match symlink_on_path(&ws, p) {
                Ok(None) => {}
                Ok(Some(link)) => return Err(format!("path {p} crosses symlink {link}")),
                Err(e) => return Err(format!("cannot inspect {p}: {e}")),
            }
        }
        let actual = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;
        if actual != expected_base {
            return Err(format!("version conflict: expected {expected_base}, actual {actual}"));
        }
        // A planted `.git` would make `git apply` honour its repo-local configuration.
        purge_excluded(&ws).map_err(|e| format!("cannot clean workspace: {e}"))?;

        for args in [&["apply", "--check"][..], &["apply"][..]] {
            let out = git(&ws)
                .args(args)
                .arg(&patch_file)
                .output()
                .await
                .map_err(|e| format!("cannot run git: {e}"))?;
            if !out.status.success() {
                return Err(format!("patch does not apply: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
        }
        let digest = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;
        Ok((paths, digest))
    }

    async fn run_verification(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match self.try_run_verification(req).await {
            Ok((evidence, report)) => {
                let mut out = ExecOutcome::success(req, ctx, evidence.to_string().into_bytes());
                out.verification = Some(report);
                out
            }
            Err(reason) => ExecOutcome::failure(req, ctx, reason),
        }
    }

    /// Runs the profile from a fresh per-run copy outside the workspace, so code under test
    /// cannot rewrite the protected source; any change to either copy voids the evidence.
    async fn try_run_verification(
        &self,
        req: &EffectRequest,
    ) -> Result<(serde_json::Value, VerificationReport), String> {
        let ws = self.workspace(&req.task_id);
        if !ws.is_dir() {
            return Err("workspace missing: no snapshot was read".into());
        }
        let raw = std::fs::read(self.profile_dir.join("profile.json"))
            .map_err(|e| format!("cannot read profile: {e}"))?;
        let profile: Profile = serde_json::from_slice(&raw).map_err(|e| format!("invalid profile.json: {e}"))?;
        let Some((program, args)) = profile.command.split_first() else {
            return Err("profile command is empty".into());
        };
        let (ws_real, profile_real) = (
            ws.canonicalize().map_err(|e| e.to_string())?,
            self.profile_dir.canonicalize().map_err(|e| e.to_string())?,
        );
        if profile_real.starts_with(&ws_real) || ws_real.starts_with(&profile_real) {
            return Err("profile and workspace overlap".into());
        }

        let profile_digest = workspace_digest(&self.profile_dir).map_err(|e| format!("cannot digest profile: {e}"))?;
        // Entries the digest ignores must not decide the check: a bytecode cache planted by
        // an earlier run could stand in for the source the evidence names.
        purge_excluded(&ws).map_err(|e| format!("cannot clean workspace: {e}"))?;
        let run_dir = tempfile::tempdir_in(self.task_dir(&req.task_id)).map_err(|e| format!("scratch dir: {e}"))?;
        let run_profile = run_dir.path().join("profile");
        copy_tree(&self.profile_dir, &run_profile).map_err(|e| format!("cannot stage profile: {e}"))?;
        if let Some(pinned) = self.pinned_profile {
            let staged = workspace_digest(&run_profile).map_err(|e| format!("cannot digest profile: {e}"))?;
            if staged != pinned {
                return Err(format!("profile digest mismatch: pinned {pinned}, found {staged}"));
            }
        }
        let workspace = workspace_digest(&ws).map_err(|e| format!("cannot digest workspace: {e}"))?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .arg(&ws)
            .current_dir(&run_profile)
            .env_clear()
            .env("PYTHONDONTWRITEBYTECODE", "1")
            // Bytecode caches live (and are looked up) in this run's scratch dir only.
            .env("PYTHONPYCACHEPREFIX", run_dir.path().join("pycache"));
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        let output = match run_in_group(cmd, self.verify_timeout, OUTPUT_LIMIT, self.groups_file.as_deref()).await {
            Err(GroupError::Timeout) => return Err("timeout".into()),
            Err(GroupError::Io(e)) => return Err(format!("cannot run profile command: {e}")),
            Ok(o) => o,
        };

        let unchanged = |dir: &Path, want: Digest| workspace_digest(dir).is_ok_and(|d| d == want);
        if !unchanged(&self.profile_dir, profile_digest) || !unchanged(&run_profile, profile_digest) {
            return Err("protected profile changed during verification".into());
        }
        if !unchanged(&ws, workspace) {
            return Err("workspace changed during verification".into());
        }
        // Whatever the check left behind outside the digest voids its evidence.
        let polluted = excluded_entries(&ws).map_err(|e| format!("cannot inspect workspace: {e}"))?;
        if !polluted.is_empty() {
            return Err(format!("workspace polluted by excluded entries: {}", polluted.join(", ")));
        }

        let exit_code = output.status.code();
        let passed = exit_code == Some(0);
        let (stdout, stdout_truncated) = truncated(&output.stdout);
        let (stderr, stderr_truncated) = truncated(&output.stderr);
        let summary = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("exit code {exit_code:?}"));
        let evidence = json!({
            "summary": summary,
            "profile_id": profile.id,
            "profile_digest": profile_digest,
            "workspace_digest": workspace,
            "command": profile.command,
            "exit_code": exit_code,
            "passed": passed,
            "stdout": stdout,
            "stdout_truncated": stdout_truncated,
            "stderr": stderr,
            "stderr_truncated": stderr_truncated,
        });
        Ok((evidence, VerificationReport { passed, workspace, summary }))
    }
}

impl Executor for FixtureExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::ReadSnapshot => self.read_snapshot(req, ctx).await,
            EffectKind::ApplyPatch { expected_base } => self.apply_patch(req, ctx, expected_base).await,
            EffectKind::RunVerification => self.run_verification(req, ctx).await,
            EffectKind::ExportBundle => ExecOutcome::failure(req, ctx, "not implemented in this milestone"),
        }
    }

    /// Only a patch can be reconciled; the other kinds are retried or left unknown.
    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        let EffectKind::ApplyPatch { expected_base } = req.kind else {
            return Reconciliation::Unknown;
        };
        match self.patch_state(req, expected_base).await {
            Ok(None) => Reconciliation::NotApplied,
            Ok(Some((paths, digest))) => Reconciliation::Applied(FixtureExecutor::patch_applied(req, ctx, paths, digest)),
            Err(reason) => {
                tracing::warn!(effect_id = %req.effect_id, reason, "patch cannot be reconciled");
                Reconciliation::Unknown
            }
        }
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        let ws = self.workspace(task);
        Some(if ws.is_dir() {
            workspace_digest(&ws).map_err(|e| e.to_string())
        } else {
            Err(format!("workspace directory {} is missing", ws.display()))
        })
    }
}
