//! Local-directory executor: a per-task copy of a fixture snapshot, patched with `git apply`
//! and checked by a protected profile that lives outside the workspace.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentos_core::effect::EffectKind;
use agentos_core::ids::{Digest, TaskId};
use serde::Deserialize;

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
use crate::outcomes::{self, Check};
use crate::patch::{git, paths_of_file};
use crate::process::{run_in_group, GroupError};
use crate::workspace::{copy_tree, symlink_on_path, excluded_entries, has_excluded_component, purge_excluded, workspace_digest};

use agentos_core::guest::OUTPUT_LIMIT;

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
    /// Test seam: runs with the source profile directory right after the profile is staged.
    #[cfg(test)]
    after_stage: Option<fn(&Path)>,
}

/// Runs blocking filesystem work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(io::Error::other)?
}

/// At most `OUTPUT_LIMIT` bytes, and whether more were captured.
fn truncated(mut bytes: Vec<u8>) -> (Vec<u8>, bool) {
    let cut = bytes.len() > OUTPUT_LIMIT;
    bytes.truncate(OUTPUT_LIMIT);
    (bytes, cut)
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
            #[cfg(test)]
            after_stage: None,
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
            Ok((files, digest)) => outcomes::snapshot_manifest(req, ctx, files, digest),
            Err(e) => ExecOutcome::failure(req, ctx, format!("snapshot failed: {e}")),
        }
    }

    async fn apply_patch(&self, req: &EffectRequest, ctx: &AttemptCtx, expected_base: Digest) -> ExecOutcome {
        match self.try_apply_patch(req, expected_base).await {
            Ok((paths, digest)) => outcomes::patch_applied(req, ctx, paths, digest),
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
            Ok(check) => outcomes::evidence(req, ctx, &check),
            Err(reason) => ExecOutcome::failure(req, ctx, reason),
        }
    }

    /// Runs the profile from a fresh per-run copy outside the workspace, so code under test
    /// cannot rewrite the protected source. The command, id and digest all come from that
    /// staged copy, so a pin covers exactly what runs. Unpinned, any change to the source
    /// while the run is staged or under way voids the evidence; pinned, the source is not
    /// consulted after staging, and any change to the staged copy voids it.
    async fn try_run_verification(
        &self,
        req: &EffectRequest,
    ) -> Result<Check, String> {
        let ws = self.workspace(&req.task_id);
        if !ws.is_dir() {
            return Err("workspace missing: no snapshot was read".into());
        }
        let (ws_real, profile_real) = (
            ws.canonicalize().map_err(|e| e.to_string())?,
            self.profile_dir.canonicalize().map_err(|e| e.to_string())?,
        );
        if profile_real.starts_with(&ws_real) || ws_real.starts_with(&profile_real) {
            return Err("profile and workspace overlap".into());
        }

        let source_digest = match self.pinned_profile {
            Some(_) => None,
            None => Some(workspace_digest(&self.profile_dir).map_err(|e| format!("cannot digest profile: {e}"))?),
        };
        // Entries the digest ignores must not decide the check: a bytecode cache planted by
        // an earlier run could stand in for the source the evidence names.
        purge_excluded(&ws).map_err(|e| format!("cannot clean workspace: {e}"))?;
        let run_dir = tempfile::tempdir_in(self.task_dir(&req.task_id)).map_err(|e| format!("scratch dir: {e}"))?;
        let run_profile = run_dir.path().join("profile");
        copy_tree(&self.profile_dir, &run_profile).map_err(|e| format!("cannot stage profile: {e}"))?;
        #[cfg(test)]
        if let Some(hook) = self.after_stage {
            hook(&self.profile_dir);
        }
        let profile_digest = workspace_digest(&run_profile).map_err(|e| format!("cannot digest profile: {e}"))?;
        match (self.pinned_profile, source_digest) {
            (Some(pinned), _) if profile_digest != pinned => {
                return Err(format!("profile digest mismatch: pinned {pinned}, found {profile_digest}"));
            }
            (None, Some(source)) if profile_digest != source => {
                return Err("protected profile changed during verification".into());
            }
            _ => {}
        }
        // Parsed from the bytes just digested, never from the shared source.
        let raw = std::fs::read(run_profile.join("profile.json")).map_err(|e| format!("cannot read profile: {e}"))?;
        let profile: Profile = serde_json::from_slice(&raw).map_err(|e| format!("invalid profile.json: {e}"))?;
        let Some((program, args)) = profile.command.split_first() else {
            return Err("profile command is empty".into());
        };
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
        let source_changed = source_digest.is_some_and(|d| !unchanged(&self.profile_dir, d));
        if source_changed || !unchanged(&run_profile, profile_digest) {
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

        let (stdout, stdout_truncated) = truncated(output.stdout);
        let (stderr, stderr_truncated) = truncated(output.stderr);
        Ok(Check {
            profile_id: profile.id,
            command: profile.command,
            profile_digest,
            workspace_digest: workspace,
            exit_code: output.status.code(),
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
        })
    }
}

impl Executor for FixtureExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::ReadSnapshot => self.read_snapshot(req, ctx).await,
            EffectKind::ApplyPatch { expected_base } => self.apply_patch(req, ctx, expected_base).await,
            EffectKind::RunVerification => self.run_verification(req, ctx).await,
            EffectKind::ExportBundle => ExecOutcome::failure(req, ctx, "not implemented in this milestone"),
            EffectKind::ModelCall { .. } | EffectKind::ListFiles { .. } | EffectKind::ReadFile { .. } => {
                ExecOutcome::failure(req, ctx, format!("not a worker effect: {}", req.kind.tag()))
            }
        }
    }

    /// Only a patch can be reconciled; the other kinds are retried or left unknown.
    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        let EffectKind::ApplyPatch { expected_base } = req.kind else {
            return Reconciliation::Unknown;
        };
        match self.patch_state(req, expected_base).await {
            Ok(None) => Reconciliation::NotApplied,
            Ok(Some((paths, digest))) => Reconciliation::Applied(outcomes::patch_applied(req, ctx, paths, digest)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::effect::{AttemptId, EffectId, Outcome};
    use serde_json::json;

    const STAGED: &str = r#"{"id": "staged", "command": ["python3", "-c", "print('STAGED')"], "protected": true}"#;

    fn tamper(profile_dir: &Path) {
        let tampered = r#"{"id": "tampered", "command": ["python3", "-c", "print('TAMPERED')"], "protected": true}"#;
        std::fs::write(profile_dir.join("profile.json"), tampered).unwrap();
    }

    fn contract() -> Contract {
        Contract::parse(r#"{"goal":"g","repository":{"source":"s","revision":"r"},"profile":"p","editable_paths":["src/**"],"verification_profile":"v","capabilities":["snapshot.read"],"limits":{"model_requests":1,"max_output_tokens_per_request":1,"tool_actions":1,"deadline_seconds":1,"worker_vcpus":1,"worker_memory_mib":1}}"#).unwrap()
    }

    /// Runs snapshot then verification with `tamper` hooked in after staging.
    async fn verify_with_tampering(pinned: bool) -> (ExecOutcome, Digest) {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        copy_tree(&fixtures.join("parser-repo"), &dir.path().join("snapshot")).unwrap();
        std::fs::create_dir(dir.path().join("profile")).unwrap();
        std::fs::write(dir.path().join("profile/profile.json"), STAGED).unwrap();
        let digest = workspace_digest(&dir.path().join("profile")).unwrap();
        let mut exec = FixtureExecutor::new(dir.path().join("snapshot"), dir.path().join("profile"), dir.path().join("work"))
            .with_pinned_profile(pinned.then_some(digest));
        exec.after_stage = Some(tamper);
        let task = TaskId::new();
        let ctx = AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "t".into() };
        let req = |kind: EffectKind| EffectRequest {
            effect_id: EffectId::derive(&task, 0, &kind, &Digest::of(b"")),
            task_id: task.clone(),
            kind,
            payload: vec![],
            contract: contract(),
            deadline_ts: 0,
        };
        let snap = exec.run(&req(EffectKind::ReadSnapshot), &ctx).await;
        assert_eq!(snap.receipt.outcome, Outcome::Success);
        (exec.run(&req(EffectKind::RunVerification), &ctx).await, digest)
    }

    #[tokio::test]
    async fn a_pinned_verification_runs_the_staged_command_not_the_source() {
        let (out, pinned) = verify_with_tampering(true).await;
        assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
        let evidence: serde_json::Value = serde_json::from_slice(&out.output).unwrap();
        assert_eq!(evidence["stdout"].as_str().unwrap().trim(), "STAGED");
        assert_eq!(evidence["profile_id"], "staged");
        assert_eq!(evidence["command"], json!(["python3", "-c", "print('STAGED')"]));
        assert_eq!(evidence["profile_digest"], json!(pinned));
        assert!(out.verification.unwrap().passed);
    }

    #[tokio::test]
    async fn an_unpinned_verification_still_voids_evidence_when_the_source_changes() {
        let (out, _) = verify_with_tampering(false).await;
        let Outcome::Failure(reason) = &out.receipt.outcome else {
            panic!("expected failure, got {}", String::from_utf8_lossy(&out.output))
        };
        assert_eq!(reason, "protected profile changed during verification");
        assert!(out.verification.is_none());
    }
}
