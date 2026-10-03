//! Read-only serving of `ListFiles` and `ReadFile` from a *shadow workspace*: the snapshot
//! plus the task's journaled, completed patches, rebuilt on every read and checked against
//! the digest the journal says the workspace has. No live workspace is ever touched, and the
//! model-chosen path is validated and checked for symlinks before it is joined to a host
//! path.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use agentos_core::effect::EffectKind;
use agentos_core::ids::{Digest, TaskId};
use agentos_store::db::Db;
use serde_json::json;

use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::guestlink::guest_text;
use crate::job::atomic_write;
use crate::journal;
use crate::patch::git;
use crate::workspace::{copy_tree, has_excluded_component, list_files, purge_excluded, symlink_on_path, workspace_digest};

/// The most bytes of a file one read returns.
pub const READ_LIMIT: usize = 64 * 1024;

pub struct ShadowReader {
    db_path: PathBuf,
    snapshot_dir: PathBuf,
    shadow_root: PathBuf,
    crash: Option<CrashHook>,
}

/// Why `rel` (chosen by the model) can never name a file of the workspace.
pub fn check_path(rel: &str) -> Result<(), String> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("file not in the workspace: {}", guest_text(rel)));
    }
    if has_excluded_component(rel) {
        return Err(format!("path excluded from the workspace digest: {}", guest_text(rel)));
    }
    Ok(())
}

/// Reads at most `READ_LIMIT` bytes of the regular file `rel` under `shadow`:
/// `(content, truncated)`. Refuses a bad path and any symlink on it before touching it.
pub fn read_from(shadow: &Path, rel: &str) -> Result<(String, bool), String> {
    check_path(rel)?;
    let crosses = symlink_on_path(shadow, rel).map_err(|e| format!("cannot read {}: {}", guest_text(rel), guest_text(&e.to_string())))?;
    if let Some(link) = crosses {
        return Err(format!("path {} crosses symlink {}", guest_text(rel), guest_text(&link)));
    }
    let path = shadow.join(rel);
    match fs::metadata(&path) {
        Ok(m) if m.is_file() => {}
        _ => return Err(format!("file not in the workspace: {}", guest_text(rel))),
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .and_then(|f| f.take(READ_LIMIT as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("cannot read {}: {e}", guest_text(rel)))?;
    let truncated = bytes.len() > READ_LIMIT;
    let content = String::from_utf8_lossy(&bytes[..bytes.len().min(READ_LIMIT)]).into_owned();
    Ok((content, truncated))
}

impl ShadowReader {
    pub fn new(db_path: PathBuf, snapshot_dir: PathBuf, shadow_root: PathBuf) -> ShadowReader {
        ShadowReader { db_path, snapshot_dir, shadow_root, crash: None }
    }

    pub fn with_crash(mut self, hook: Option<CrashHook>) -> Self {
        self.crash = hook;
        self
    }

    /// `<shadow_root>/<task>/shadow`.
    pub fn shadow_dir(&self, task: &TaskId) -> PathBuf {
        self.shadow_root.join(task.to_string()).join("shadow")
    }

    /// Rebuilds the shadow from the snapshot and the journaled patches; returns its digest
    /// once it equals the one the journal records for the task.
    async fn rebuild(&self, task: &TaskId) -> Result<Digest, String> {
        let shadow = self.shadow_dir(task);
        let base = shadow.parent().expect("a shadow dir has a parent").to_path_buf();
        let (db_path, snapshot, t, dir) = (self.db_path.clone(), self.snapshot_dir.clone(), task.clone(), shadow.clone());
        let (expected, patches) = tokio::task::spawn_blocking(move || -> Result<(Digest, Vec<Vec<u8>>), String> {
            let db = Db::open(&db_path).map_err(|e| format!("cannot open the journal: {e}"))?;
            let expected = db.task(&t).map_err(|e| e.to_string())?.workspace_digest;
            let patches = journal::completed_patches(&db, &t).map_err(|e| e.to_string())?;
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot clear the shadow workspace: {e}")),
            }
            copy_tree(&snapshot, &dir).map_err(|e| format!("cannot copy the snapshot: {e}"))?;
            purge_excluded(&dir).map_err(|e| format!("cannot purge the shadow workspace: {e}"))?;
            Ok((expected, patches))
        })
        .await
        .map_err(|e| e.to_string())??;

        for (i, patch) in patches.iter().enumerate() {
            let file = base.join(format!("patch-{i}.diff"));
            atomic_write(&file, patch).map_err(|e| format!("cannot write shadow patch {i}: {e}"))?;
            let out = git(&shadow).args(["apply"]).arg(&file).output().await.map_err(|e| format!("cannot run git: {e}"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(format!("shadow patch {i} does not apply: {}", guest_text(stderr.trim())));
            }
        }

        let actual = tokio::task::spawn_blocking(move || -> Result<Digest, String> {
            purge_excluded(&shadow).map_err(|e| format!("cannot purge the shadow workspace: {e}"))?;
            workspace_digest(&shadow).map_err(|e| format!("cannot digest the shadow workspace: {e}"))
        })
        .await
        .map_err(|e| e.to_string())??;
        if actual != expected {
            return Err(format!("shadow workspace digest mismatch: expected {expected}, found {actual}"));
        }
        Ok(actual)
    }

    async fn serve(&self, req: &EffectRequest) -> Result<(Vec<u8>, &'static str), String> {
        match &req.kind {
            EffectKind::ListFiles { .. } => {
                let digest = self.rebuild(&req.task_id).await?;
                let shadow = self.shadow_dir(&req.task_id);
                let files: Vec<String> = list_files(&shadow)
                    .map_err(|e| format!("cannot list the shadow workspace: {e}"))?
                    .into_iter()
                    .map(|(rel, _)| rel)
                    .collect();
                Ok((serde_json::to_vec(&json!({ "files": files, "workspace_digest": digest })).expect("json"), "list_files"))
            }
            EffectKind::ReadFile { path, .. } => {
                // The path is vetted before any rebuild: a bad one costs nothing.
                check_path(path)?;
                let digest = self.rebuild(&req.task_id).await?;
                let (content, truncated) = read_from(&self.shadow_dir(&req.task_id), path)?;
                let body = json!({ "path": path, "content": content, "truncated": truncated, "workspace_digest": digest });
                Ok((serde_json::to_vec(&body).expect("json"), "read_file"))
            }
            _ => Err("not a read effect".into()),
        }
    }
}

impl Executor for ShadowReader {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match self.serve(req).await {
            Ok((output, tag)) => {
                if self.crash.as_ref().is_some_and(|h| h.check(CrashPoint::DuringExecute, Some(tag))) {
                    return ExecOutcome::failure(req, ctx, "injected crash after the read");
                }
                ExecOutcome::success(req, ctx, output)
            }
            Err(why) => ExecOutcome::failure(req, ctx, why),
        }
    }
}
