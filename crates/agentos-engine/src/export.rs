//! Export bundle of a finished task: what was asked, what changed, and the evidence that
//! it was checked, each named by a digest that is re-verified from its bytes here.
//!
//! Layout of the bundle directory:
//! - `manifest.json`: [`Manifest`];
//! - `patch.diff`: the successfully applied patches, concatenated in application order.
//!   `git apply patch.diff` on a pristine copy of the base snapshot reproduces the final
//!   workspace (git applies later diffs of a file on top of the earlier ones);
//! - `patches/NNNN-<digest>.patch`: each applied patch on its own;
//! - `evidence/<digest>.json`: the snapshot manifest and every verification result;
//! - `model/NNNN-request.json`, `model/NNNN-response.json`: the request sent for each finished
//!   model call and the response it got (none for a call whose answer was lost), in intent
//!   order, as listed in [`Manifest::model_calls`].
//!
//! Each verification result reports `passed`, the check's own verdict from its evidence,
//! and `accepted_for_final_workspace`, the engine's decision: only the result whose
//! completion made the task SUCCEEDED is accepted. A check that passed but was not accepted
//! (e.g. a cancel landed while it ran) is evidence, not a success claim.
//!
//! The bundle is written into a temp directory beside the destination and renamed into
//! place, so a failure at any point leaves no partial bundle.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentos_core::effect::{EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError, StoredEvent};
use agentos_store::effects::UsageSummary;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::journal;
use crate::runner::EngineError;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("task is {}, only finished tasks can be exported", .0.label())]
    NotTerminal(TaskState),
    #[error("destination {0} exists and is not an empty directory")]
    DestinationNotEmpty(PathBuf),
    #[error("destination {0} has no parent directory")]
    NoParent(PathBuf),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("journal is inconsistent: {0}")]
    Inconsistent(String),
}

impl From<EngineError> for ExportError {
    fn from(e: EngineError) -> ExportError {
        match e {
            EngineError::Db(e) => ExportError::Db(e),
            EngineError::Blob(e) => ExportError::Io(e),
            other => ExportError::Inconsistent(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, ExportError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchEntry {
    pub effect_id: EffectId,
    pub digest: Digest,
    /// Path inside the bundle.
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCallEntry {
    pub effect_id: EffectId,
    pub request_digest: Digest,
    /// `None` for a call that failed without an answer (lost, then forfeited).
    pub response_digest: Option<Digest>,
    /// `COMPLETED` or `FAILED`.
    pub state: String,
    /// Path inside the bundle.
    pub request_file: String,
    pub response_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub effect_id: EffectId,
    /// False when the check did not run to completion (e.g. a timeout).
    pub completed: bool,
    pub passed: bool,
    pub workspace_digest: Option<Digest>,
    /// The result blob, in the bundle as `evidence/<digest>.json`.
    pub evidence_digest: Digest,
    pub profile_digest: Option<Digest>,
    pub exit_code: Option<i64>,
    /// The engine's decision, as opposed to `passed`, the check's own verdict: true only for
    /// the result whose completion committed `VerifyPassed`, so it is the evidence behind
    /// the task's `verified_digest` (it then passed, for the final workspace).
    pub accepted_for_final_workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub task_id: TaskId,
    /// `TaskState::label`; only `SUCCEEDED` claims a verified result.
    pub state: String,
    /// The repository digest recorded at submission, or the contract's revision for a task
    /// that has no submission record.
    pub base_revision: String,
    /// Digest of the snapshot the task worked on; `None` if it never read one.
    pub base_workspace_digest: Option<Digest>,
    /// Digest of `patch.diff`.
    pub patch_digest: Digest,
    pub patches: Vec<PatchEntry>,
    /// The workspace after the last applied patch (the snapshot if none applied).
    pub final_workspace_digest: Option<Digest>,
    /// Set only for a SUCCEEDED task: the workspace the passing verification checked.
    pub verified_digest: Option<Digest>,
    pub verification_profile_digest: Option<Digest>,
    pub verification_results: Vec<VerificationResult>,
    /// The model calls that finished, in intent order.
    #[serde(default)]
    pub model_calls: Vec<ModelCallEntry>,
    pub usage_summary: UsageSummary,
    pub contract_digest: Digest,
    /// The model recorded at submission, if any.
    pub model: Option<String>,
    #[serde(default)]
    pub model_policy_version: u32,
    #[serde(default)]
    pub model_limits_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_endpoint: Option<String>,
    /// Journal events of the task at export time.
    pub generated_events: usize,
    /// The task's capability handles, by 8-character prefix only (a full handle never
    /// leaves the database).
    #[serde(default)]
    pub capabilities: Vec<CapabilityEntry>,
    /// The guest image a Firecracker task ran on (`Submitted.guest_image_digest`); absent for
    /// host tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_image_digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub operation: String,
    pub handle_prefix: String,
    pub revoked: bool,
}

fn inconsistent(msg: impl Into<String>) -> ExportError {
    ExportError::Inconsistent(msg.into())
}

/// Blob bytes, re-verified against their digest.
fn read_blob(blobs: &BlobStore, d: &Digest) -> Result<Vec<u8>> {
    let bytes = blobs.get(d).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidData => ExportError::Integrity(e.to_string()),
        _ => ExportError::Io(e),
    })?;
    if Digest::of(&bytes) != *d {
        return Err(ExportError::Integrity(format!("blob {d} does not match its digest")));
    }
    Ok(bytes)
}

fn parse(what: &str, bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| inconsistent(format!("{what} is not JSON: {e}")))
}

fn digest_field(v: &Value, field: &str, what: &str) -> Result<Digest> {
    let s = v[field].as_str().ok_or_else(|| inconsistent(format!("{what} has no {field}")))?;
    Digest::from_hex(s).map_err(|e| inconsistent(format!("{what}: {field}: {e}")))
}

fn optional_digest(v: &Value, field: &str, what: &str) -> Result<Option<Digest>> {
    if v.get(field).is_none_or(Value::is_null) {
        return Ok(None);
    }
    digest_field(v, field, what).map(Some)
}

fn result_of(rec: &EffectRecord) -> Result<Digest> {
    rec.result_digest.ok_or_else(|| inconsistent(format!("finished effect {} has no result", rec.effect_id)))
}

/// Effects in intent order.
fn effects(db: &Db, events: &[StoredEvent]) -> Result<Vec<EffectRecord>> {
    events
        .iter()
        .filter(|e| e.event_type == "EffectIntended")
        .map(|e| {
            let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).map_err(DbError::from)?;
            Ok(db.effect(&id)?)
        })
        .collect()
}

/// The text of an applied patch: from its journaled agent turn, else its blob.
fn patch_text(db: &Db, blobs: &BlobStore, task: &TaskId, rec: &EffectRecord) -> Result<Vec<u8>> {
    let bytes = match journal::journaled_patch(db, task, &rec.request_digest)? {
        Some(text) => text.into_bytes(),
        None if blobs.exists(&rec.request_digest) => read_blob(blobs, &rec.request_digest)?,
        None => return Err(inconsistent(format!("the patch of effect {} is gone", rec.effect_id))),
    };
    if Digest::of(&bytes) != rec.request_digest {
        return Err(ExportError::Integrity(format!("patch of effect {} does not match its digest", rec.effect_id)));
    }
    Ok(bytes)
}

/// Everything the bundle holds, gathered and checked before anything is written.
struct Contents {
    manifest: Manifest,
    patch_diff: Vec<u8>,
    patches: Vec<(String, Vec<u8>)>,
    /// `model/NNNN-request.json` and `-response.json` files, in order.
    model_files: Vec<(String, Vec<u8>)>,
    evidence: BTreeMap<Digest, Vec<u8>>,
}

fn collect(db: &Db, blobs: &BlobStore, task: &TaskId) -> Result<Contents> {
    let t = db.task(task)?;
    if !t.state.is_terminal() {
        return Err(ExportError::NotTerminal(t.state));
    }
    let events = db.events(task)?;
    let contract = db.contract(task)?;
    let created = events.iter().find(|e| e.event_type == "TaskCreated").ok_or_else(|| inconsistent("no TaskCreated event"))?;
    let contract_digest = digest_field(&created.payload, "contract_digest", "TaskCreated")?;
    let submitted = events.iter().find(|e| e.event_type == "Submitted").map(|e| &e.payload);
    let (submitted_repo, submitted_profile, guest_image_digest) = match submitted {
        Some(s) => {
            // Submission digests the stored contract serialization, so it can be re-checked.
            if Digest::of(&serde_json::to_vec(&contract).map_err(DbError::from)?) != contract_digest {
                return Err(inconsistent(format!("contract digest {contract_digest} does not match the stored contract")));
            }
            (
                Some(digest_field(s, "repository_digest", "Submitted")?),
                optional_digest(s, "profile_digest", "Submitted")?,
                optional_digest(s, "guest_image_digest", "Submitted")?,
            )
        }
        None => (None, None, None),
    };

    let mut evidence = BTreeMap::new();
    let (mut base, mut last) = (None, None);
    let (mut patch_diff, mut patches, mut entries) = (Vec::new(), Vec::new(), Vec::new());
    let mut results = Vec::new();
    let (mut model_calls, mut model_files) = (Vec::new(), Vec::new());
    for rec in effects(db, &events)? {
        match (&rec.kind, rec.state) {
            (EffectKind::ReadSnapshot, EffectState::Completed) => {
                let d = result_of(&rec)?;
                let bytes = read_blob(blobs, &d)?;
                let ws = digest_field(&parse("snapshot manifest", &bytes)?, "workspace_digest", "snapshot manifest")?;
                (base, last) = (Some(ws), Some(ws));
                evidence.insert(d, bytes);
            }
            (EffectKind::ApplyPatch { .. }, EffectState::Completed) => {
                let result = read_blob(blobs, &result_of(&rec)?)?;
                last = Some(digest_field(&parse("patch result", &result)?, "workspace_digest", "patch result")?);
                let text = patch_text(db, blobs, task, &rec)?;
                if !patch_diff.is_empty() && !patch_diff.ends_with(b"\n") {
                    patch_diff.push(b'\n');
                }
                patch_diff.extend_from_slice(&text);
                let file = format!("patches/{:04}-{}.patch", entries.len() + 1, rec.request_digest);
                entries.push(PatchEntry { effect_id: rec.effect_id.clone(), digest: rec.request_digest, file: file.clone() });
                patches.push((file, text));
            }
            (EffectKind::RunVerification, EffectState::Completed | EffectState::Failed) => {
                let d = result_of(&rec)?;
                let bytes = read_blob(blobs, &d)?;
                let v = parse("verification result", &bytes)?;
                let completed = rec.state == EffectState::Completed;
                let what = format!("evidence {d}");
                results.push(VerificationResult {
                    effect_id: rec.effect_id.clone(),
                    completed,
                    passed: completed && v["passed"] == true,
                    workspace_digest: if completed { Some(digest_field(&v, "workspace_digest", &what)?) } else { None },
                    evidence_digest: d,
                    profile_digest: if completed { Some(digest_field(&v, "profile_digest", &what)?) } else { None },
                    exit_code: if completed { v["exit_code"].as_i64() } else { None },
                    accepted_for_final_workspace: false,
                });
                evidence.insert(d, bytes);
            }
            (EffectKind::ModelCall { .. }, EffectState::Completed | EffectState::Failed) => {
                let n = model_calls.len() + 1;
                let request = match blobs.exists(&rec.request_digest) {
                    true => read_blob(blobs, &rec.request_digest)?,
                    false => return Err(inconsistent(format!("the request of model call {} is gone", rec.effect_id))),
                };
                let request_file = format!("model/{n:04}-request.json");
                model_files.push((request_file.clone(), request));
                let response_file = match rec.result_digest {
                    Some(d) => {
                        let file = format!("model/{n:04}-response.json");
                        model_files.push((file.clone(), read_blob(blobs, &d)?));
                        Some(file)
                    }
                    None => None,
                };
                model_calls.push(ModelCallEntry {
                    effect_id: rec.effect_id.clone(),
                    request_digest: rec.request_digest,
                    response_digest: rec.result_digest,
                    state: if rec.state == EffectState::Completed { "COMPLETED" } else { "FAILED" }.to_string(),
                    request_file,
                    response_file,
                });
            }
            // File listings and reads are not exported.
            _ => {}
        }
    }

    if let (Some(recorded), Some(read)) = (submitted_repo, base)
        && recorded != read
    {
        return Err(inconsistent(format!("the snapshot read ({read}) is not the submitted repository ({recorded})")));
    }
    let profile = submitted_profile.or_else(|| results.iter().find_map(|r| r.profile_digest));
    if let Some(r) = results.iter().find(|r| r.profile_digest.is_some_and(|p| Some(p) != profile)) {
        return Err(inconsistent(format!(
            "verification {} ran profile {:?}, not the submitted profile {profile:?}",
            r.effect_id, r.profile_digest
        )));
    }
    // The verification whose completion committed VerifyPassed (same transaction, so its
    // EffectCompleted is the event right before).
    let accepting = events.windows(2).find(|w| w[1].event_type == "VerifyPassed" && w[0].event_type == "EffectCompleted");
    let accepting: Option<EffectId> = accepting
        .map(|w| serde_json::from_value(w[0].payload["effect_id"].clone()).map_err(DbError::from))
        .transpose()?;
    for r in &mut results {
        r.accepted_for_final_workspace = t.state == TaskState::Succeeded
            && accepting.as_ref() == Some(&r.effect_id)
            && r.passed
            && r.workspace_digest.is_some()
            && r.workspace_digest == t.verified_digest
            && r.workspace_digest == last;
    }
    let verified = if t.state == TaskState::Succeeded {
        let v = t.verified_digest.ok_or_else(|| inconsistent("SUCCEEDED without a verified digest"))?;
        let evidenced = results.iter().any(|r| r.accepted_for_final_workspace);
        if last != Some(v) || t.workspace_digest != v || !evidenced {
            return Err(inconsistent(format!("verified digest {v} is not the final workspace {last:?} with passing evidence")));
        }
        Some(v)
    } else {
        None
    };

    let manifest = Manifest {
        task_id: task.clone(),
        state: t.state.label().to_string(),
        base_revision: submitted_repo.map_or_else(|| contract.repository.revision.clone(), |d| d.to_string()),
        base_workspace_digest: base,
        patch_digest: Digest::of(&patch_diff),
        patches: entries,
        final_workspace_digest: last,
        verified_digest: verified,
        verification_profile_digest: profile,
        verification_results: results,
        model_calls,
        usage_summary: db.usage_summary(task)?,
        contract_digest,
        model: submitted.and_then(|s| s["model"].as_str()).map(str::to_string),
        model_policy_version: crate::model::policy::versions(db, task)?.0,
        model_limits_version: crate::model::policy::versions(db, task)?.1,
        model_endpoint: submitted.filter(|s| s["model"].as_str().is_some_and(|m| m.starts_with("anthropic:")))
            .map(|s| s["model_endpoint"].as_str().unwrap_or(crate::model::anthropic::ANTHROPIC_BASE_URL).to_string()),
        generated_events: events.len(),
        capabilities: db
            .grants(task)?
            .into_iter()
            .map(|g| CapabilityEntry {
                operation: serde_json::to_value(g.operation).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
                handle_prefix: g.handle.prefix().to_string(),
                revoked: g.revoked,
            })
            .collect(),
        guest_image_digest,
    };
    Ok(Contents { manifest, patch_diff, patches, model_files, evidence })
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create_new(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Called before each file of the bundle is written; an error aborts the export there.
type BeforeWrite<'a> = &'a dyn Fn(&str) -> io::Result<()>;

/// Writes `files` under `root`, then reads each back and checks its digest.
fn write_checked(root: &Path, files: &[(String, &[u8])], before: BeforeWrite) -> Result<()> {
    for (rel, bytes) in files {
        before(rel)?;
        write_synced(&root.join(rel), bytes)?;
    }
    for (rel, bytes) in files {
        if Digest::of(&fs::read(root.join(rel))?) != Digest::of(bytes) {
            return Err(ExportError::Integrity(format!("{rel} changed while it was written")));
        }
    }
    Ok(())
}

fn ensure_free(out_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(out_dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if m.is_dir() && fs::read_dir(out_dir)?.next().is_none() => Ok(()),
        Ok(_) => Err(ExportError::DestinationNotEmpty(out_dir.to_path_buf())),
    }
}

/// Writes the bundle of the finished `task` to `out_dir` (which must not exist, or be an
/// empty directory) and returns its manifest. See the module docs.
///
/// A successful export is journaled as an `Exported` audit event `{manifest_digest, dir,
/// files}` (`dir` as given); a refused or failed one journals nothing. If that journal write
/// itself fails, the error is returned although the bundle is in place.
pub fn export_bundle(db: &Db, blobs: &BlobStore, task: &TaskId, out_dir: &Path) -> Result<Manifest> {
    let written = write_bundle(collect(db, blobs, task)?, out_dir, &|_| Ok(()))?;
    let payload = serde_json::json!({
        "manifest_digest": written.manifest_digest,
        "dir": out_dir.display().to_string(),
        "files": written.files,
    });
    db.append_audit(task, "Exported", &payload)?;
    Ok(written.manifest)
}

#[derive(Debug)]
struct Written {
    manifest: Manifest,
    manifest_digest: Digest,
    files: usize,
}

fn write_bundle(contents: Contents, out_dir: &Path, before: BeforeWrite) -> Result<Written> {
    let parent = match out_dir.parent() {
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => return Err(ExportError::NoParent(out_dir.to_path_buf())),
    };
    ensure_free(out_dir)?;

    let tmp = tempfile::Builder::new().prefix(".agentos-export-").tempdir_in(parent)?;
    let root = tmp.path();
    fs::create_dir(root.join("patches"))?;
    fs::create_dir(root.join("evidence"))?;
    if !contents.model_files.is_empty() {
        fs::create_dir(root.join("model"))?;
    }
    let mut files: Vec<(String, &[u8])> = vec![("patch.diff".into(), &contents.patch_diff)];
    files.extend(contents.patches.iter().map(|(rel, bytes)| (rel.clone(), bytes.as_slice())));
    files.extend(contents.model_files.iter().map(|(rel, bytes)| (rel.clone(), bytes.as_slice())));
    files.extend(contents.evidence.iter().map(|(d, bytes)| (format!("evidence/{d}.json"), bytes.as_slice())));
    let manifest_json = serde_json::to_vec_pretty(&contents.manifest).map_err(DbError::from)?;
    files.push(("manifest.json".into(), &manifest_json));
    write_checked(root, &files, before)?;
    let files = files.len();
    for dir in ["patches", "evidence", "model", ""] {
        if root.join(dir).exists() {
            sync_dir(&root.join(dir))?;
        }
    }
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o755))?;
    }

    // Renaming onto an empty directory replaces it; onto a non-empty one it fails.
    let staged = tmp.keep();
    if let Err(e) = fs::rename(&staged, out_dir) {
        let _ = fs::remove_dir_all(&staged);
        return Err(match e.kind() {
            io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotADirectory => {
                ExportError::DestinationNotEmpty(out_dir.to_path_buf())
            }
            _ => e.into(),
        });
    }
    sync_dir(parent)?;
    tracing::info!(task_id = %contents.manifest.task_id, dir = %out_dir.display(), "bundle exported");
    Ok(Written { manifest: contents.manifest, manifest_digest: Digest::of(&manifest_json), files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents() -> Contents {
        let manifest = Manifest {
            task_id: TaskId::new(),
            state: "SUCCEEDED".into(),
            base_revision: "rev".into(),
            base_workspace_digest: None,
            patch_digest: Digest::of(b"diff"),
            patches: Vec::new(),
            final_workspace_digest: None,
            verified_digest: None,
            verification_profile_digest: None,
            verification_results: Vec::new(),
            model_calls: Vec::new(),
            usage_summary: UsageSummary::default(),
            contract_digest: Digest::of(b"contract"),
            model: None,
            model_policy_version: 0,
            model_limits_version: 0,
            model_endpoint: None,
            generated_events: 1,
            capabilities: Vec::new(),
            guest_image_digest: None,
        };
        let evidence = [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()].into_iter().map(|b| (Digest::of(&b), b)).collect();
        Contents { manifest, patch_diff: b"diff".to_vec(), patches: vec![("patches/0001-x.patch".into(), b"p".to_vec())], model_files: Vec::new(), evidence }
    }

    fn names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_write_failing_mid_way_leaves_no_partial_bundle() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("bundle");
        let written = std::cell::RefCell::new(Vec::new());
        let fail_on_second_evidence = |rel: &str| {
            written.borrow_mut().push(rel.to_string());
            match written.borrow().iter().filter(|r| r.starts_with("evidence/")).count() {
                2 => Err(io::Error::other("disk full")),
                _ => Ok(()),
            }
        };

        let err = write_bundle(contents(), &out, &fail_on_second_evidence).unwrap_err();

        assert!(matches!(err, ExportError::Io(ref e) if e.to_string() == "disk full"), "{err:?}");
        assert_eq!(written.borrow().len(), 4, "patch.diff, the patch and one evidence file were written first");
        assert!(names(root.path()).is_empty(), "no bundle and no temp directory remain");
    }

    #[test]
    fn the_bundle_appears_whole() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("bundle");
        let written = write_bundle(contents(), &out, &|_| Ok(())).unwrap();
        assert_eq!(written.files, 5);
        assert_eq!(written.manifest_digest, Digest::of(&fs::read(out.join("manifest.json")).unwrap()));
        let manifest = written.manifest;
        let mut got = names(&out);
        got.sort();
        assert_eq!(got, vec!["evidence", "manifest.json", "patch.diff", "patches"]);
        assert_eq!(names(&out.join("evidence")).len(), 2);
        let on_disk: Manifest = serde_json::from_slice(&fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(on_disk, manifest);
        assert_eq!(names(root.path()), vec!["bundle"]);
    }
}
