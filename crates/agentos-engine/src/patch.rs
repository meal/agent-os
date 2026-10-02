//! Patch inspection through `git apply`, so the paths we check are exactly the paths git
//! would write.

use std::path::Path;
use std::process::Output;

use agentos_core::patchrules::{check_summary, parse_numstat};
use tokio::process::Command;

/// A `git` invocation with a scrubbed environment that never discovers a repository above
/// `cwd`, so `git apply` behaves the same wherever the workspace lives.
pub(crate) fn git(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env_clear()
        .env("GIT_CEILING_DIRECTORIES", cwd.parent().unwrap_or(cwd))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

/// Repo-relative paths `patch_file` touches, in patch order, or why it is unacceptable.
/// `cwd` must be a directory outside any repository (the patch is only parsed there).
pub(crate) async fn paths_of_file(patch_file: &Path, cwd: &Path) -> Result<Vec<String>, String> {
    let numstat = git(cwd)
        .args(["apply", "--numstat", "-z"])
        .arg(patch_file)
        .output()
        .await
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !numstat.status.success() {
        return Err(format!("invalid patch: {}", stderr_of(&numstat)));
    }
    let summary = git(cwd)
        .args(["apply", "--summary"])
        .arg(patch_file)
        .output()
        .await
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !summary.status.success() {
        return Err(format!("invalid patch: {}", stderr_of(&summary)));
    }
    check_summary(&summary.stdout)?;
    parse_numstat(&numstat.stdout)
}

/// Repo-relative paths a unified diff touches, or why it is not acceptable.
pub async fn patch_paths(patch: &str) -> Result<Vec<String>, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("cannot create scratch dir: {e}"))?;
    let file = dir.path().join("change.patch");
    std::fs::write(&file, patch).map_err(|e| format!("cannot write patch: {e}"))?;
    paths_of_file(&file, dir.path()).await
}
