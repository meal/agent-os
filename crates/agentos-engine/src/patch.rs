//! Patch inspection through `git apply`, so the paths we check are exactly the paths git
//! would write.

use std::path::Path;
use std::process::Output;

use tokio::process::Command;

/// Summary lines `git apply --summary` may print for an acceptable patch: plain file
/// creation and deletion. Renames, copies, mode changes and symlinks are refused.
const ALLOWED_SUMMARY: [&str; 4] = [
    "create mode 100644 ",
    "create mode 100755 ",
    "delete mode 100644 ",
    "delete mode 100755 ",
];

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
    for line in String::from_utf8_lossy(&summary.stdout).lines() {
        let line = line.trim_start();
        if !line.is_empty() && !ALLOWED_SUMMARY.iter().any(|p| line.starts_with(p)) {
            return Err(format!("unsupported patch operation: {line}"));
        }
    }

    let mut paths = Vec::new();
    for record in numstat.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).map_err(|_| "patch path is not UTF-8".to_string())?;
        let mut fields = record.splitn(3, '\t');
        let (added, deleted, path) = match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(d), Some(p)) => (a, d, p),
            _ => return Err(format!("unexpected numstat record {record:?}")),
        };
        if added == "-" || deleted == "-" {
            return Err(format!("binary patches are not supported: {path}"));
        }
        if path.is_empty() {
            return Err(format!("unsupported patch operation in record {record:?}"));
        }
        paths.push(path.to_string());
    }
    if paths.is_empty() {
        return Err("patch touches no files".into());
    }
    Ok(paths)
}

/// Repo-relative paths a unified diff touches, or why it is not acceptable.
pub async fn patch_paths(patch: &str) -> Result<Vec<String>, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("cannot create scratch dir: {e}"))?;
    let file = dir.path().join("change.patch");
    std::fs::write(&file, patch).map_err(|e| format!("cannot write patch: {e}"))?;
    paths_of_file(&file, dir.path()).await
}
