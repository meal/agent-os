//! The bytes of successful outcomes, built on the host from structured results: one source
//! for the host worker (`FixtureExecutor`) and the Firecracker worker, so artifacts are
//! byte-identical whichever worker produced them.

use agentos_core::ids::Digest;
use serde_json::json;

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, VerificationReport};

/// A finished check: what ran, against what, and at most `OUTPUT_LIMIT` bytes of each
/// stream with whether it was cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub profile_id: String,
    pub command: Vec<String>,
    pub profile_digest: Digest,
    pub workspace_digest: Digest,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

/// `ReadSnapshot`: `{"files","workspace_digest"}`.
pub(crate) fn snapshot_manifest(req: &EffectRequest, ctx: &AttemptCtx, files: Vec<String>, digest: Digest) -> ExecOutcome {
    let output = json!({ "files": files, "workspace_digest": digest });
    let mut out = ExecOutcome::success(req, ctx, output.to_string().into_bytes());
    out.new_workspace = Some(digest);
    out
}

/// `ApplyPatch`: `{"applied":true,"paths","workspace_digest"}`; reconciliation rebuilds the
/// same bytes.
pub(crate) fn patch_applied(req: &EffectRequest, ctx: &AttemptCtx, paths: Vec<String>, digest: Digest) -> ExecOutcome {
    let output = json!({ "applied": true, "paths": paths, "workspace_digest": digest });
    let mut out = ExecOutcome::success(req, ctx, output.to_string().into_bytes());
    out.new_workspace = Some(digest);
    out
}

/// `RunVerification`: the evidence object. `passed` is decided here, from the exit code
/// alone, never from anything the check printed.
pub(crate) fn evidence(req: &EffectRequest, ctx: &AttemptCtx, check: &Check) -> ExecOutcome {
    let passed = check.exit_code == Some(0);
    let stdout = String::from_utf8_lossy(&check.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&check.stderr).into_owned();
    let summary = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("exit code {:?}", check.exit_code));
    let evidence = json!({
        "summary": summary,
        "profile_id": check.profile_id,
        "profile_digest": check.profile_digest,
        "workspace_digest": check.workspace_digest,
        "command": check.command,
        "exit_code": check.exit_code,
        "passed": passed,
        "stdout": stdout,
        "stdout_truncated": check.stdout_truncated,
        "stderr": stderr,
        "stderr_truncated": check.stderr_truncated,
    });
    let mut out = ExecOutcome::success(req, ctx, evidence.to_string().into_bytes());
    out.verification = Some(VerificationReport { passed, workspace: check.workspace_digest, summary });
    out
}
