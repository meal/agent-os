//! Export bundles: refused for unfinished tasks, written atomically (never a partial
//! directory), with every digest re-verified from the bytes it names.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{AgentAction, FakeAgent};
use agentos_engine::export::{export_bundle, ExportError, Manifest};
use agentos_engine::runner::run_task;
use agentos_engine::workspace::{copy_tree, workspace_digest};
use common::{comment_patch, fix_patch, Env};
use serde_json::json;

async fn run(env: &Env, task: &TaskId, agent: &mut FakeAgent) -> TaskState {
    run_task(&env.db, &env.blobs, &env.exec, agent, task).await.unwrap()
}

async fn succeeded(env: &Env) {
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    assert_eq!(run(env, &env.task, &mut agent).await, TaskState::Succeeded);
}

fn export(env: &Env, task: &TaskId, out: &Path) -> Result<Manifest, ExportError> {
    export_bundle(&env.db, &env.blobs, task, out)
}

/// Entries of `dir` (names), sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn blob_path(env: &Env, d: &Digest) -> PathBuf {
    let hex = d.to_string();
    env.dir.path().join("blobs/objects").join(&hex[..2]).join(&hex[2..])
}

/// `git apply` of `patch` inside `dir`, isolated from any enclosing repository.
fn git_apply(dir: &Path, patch: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("apply")
        .arg(patch)
        .output()
        .unwrap();
    assert!(out.status.success(), "git apply failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Applies the bundle's `patch.diff` to a pristine copy of the snapshot; returns its digest.
fn replay(env: &Env, bundle: &Path) -> Digest {
    let scratch = tempfile::tempdir().unwrap();
    let ws = scratch.path().join("ws");
    copy_tree(&env.snapshot_dir(), &ws).unwrap();
    git_apply(&ws, &bundle.join("patch.diff"));
    workspace_digest(&ws).unwrap()
}

fn read_manifest(bundle: &Path) -> Manifest {
    serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn succeeded_task_bundle_carries_verified_digests_that_match_the_bytes() {
    let env = Env::new(10);
    succeeded(&env).await;
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");

    let manifest = export(&env, &env.task, &bundle).unwrap();

    assert_eq!(read_manifest(&bundle), manifest, "manifest.json is the returned manifest");
    assert_eq!(entries(out_root.path()), vec!["bundle"], "no temp directory left beside the bundle");
    let task = env.db.task(&env.task).unwrap();
    assert_eq!(manifest.task_id, env.task);
    assert_eq!(manifest.state, "SUCCEEDED");
    assert_eq!(manifest.base_revision, "rev-1", "without a Submitted record the contract revision is reported");
    assert_eq!(manifest.base_workspace_digest, Some(workspace_digest(&env.snapshot_dir()).unwrap()));
    assert_eq!(manifest.final_workspace_digest, Some(env.ws_digest()));
    assert_eq!(manifest.verified_digest, task.verified_digest);
    assert_eq!(manifest.final_workspace_digest, task.verified_digest);
    assert_eq!(manifest.verification_profile_digest, Some(workspace_digest(&env.profile_dir()).unwrap()));
    assert_eq!(manifest.usage_summary, env.db.usage_summary(&env.task).unwrap());
    let created = &env.events()[0];
    assert_eq!(created.event_type, "TaskCreated");
    assert_eq!(json!(manifest.contract_digest), created.payload["contract_digest"]);
    assert_eq!(manifest.model, None, "no model was recorded at submission");
    assert_eq!(manifest.generated_events, env.events().len());

    // patch.diff and the per-patch files.
    let diff = fs::read(bundle.join("patch.diff")).unwrap();
    assert_eq!(diff, fix_patch().into_bytes());
    assert_eq!(manifest.patch_digest, Digest::of(&diff));
    let applied = env.effects("ApplyPatch");
    assert_eq!(manifest.patches.len(), 1);
    let entry = &manifest.patches[0];
    assert_eq!(entry.effect_id, applied[0].effect_id);
    assert_eq!(entry.digest, applied[0].request_digest);
    assert_eq!(entry.file, format!("patches/0001-{}.patch", entry.digest));
    assert_eq!(Digest::of(&fs::read(bundle.join(&entry.file)).unwrap()), entry.digest);

    // Verification evidence and the snapshot manifest, named by digest.
    let verifications = env.effects("RunVerification");
    assert_eq!(manifest.verification_results.len(), 1);
    let result = &manifest.verification_results[0];
    assert_eq!(result.effect_id, verifications[0].effect_id);
    assert!(result.completed && result.passed);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(Some(result.evidence_digest), verifications[0].result_digest);
    assert_eq!(result.workspace_digest, task.verified_digest);
    assert_eq!(result.profile_digest, manifest.verification_profile_digest);
    let snapshot = env.effects("ReadSnapshot")[0].result_digest.unwrap();
    let mut expected = vec![format!("{}.json", result.evidence_digest), format!("{snapshot}.json")];
    expected.sort();
    assert_eq!(entries(&bundle.join("evidence")), expected);
    for name in expected {
        let bytes = fs::read(bundle.join("evidence").join(&name)).unwrap();
        assert_eq!(format!("{}.json", Digest::of(&bytes)), name);
    }
    assert_eq!(entries(&bundle), vec!["evidence", "manifest.json", "patch.diff", "patches"]);

    assert_eq!(Some(replay(&env, &bundle)), manifest.final_workspace_digest, "patch.diff reproduces the workspace");
}

#[tokio::test]
async fn patch_diff_concatenates_patches_to_the_same_file_and_reproduces_the_workspace() {
    let env = Env::new(10);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(comment_patch()),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);
    assert_eq!(run(&env, &env.task, &mut agent).await, TaskState::Succeeded);
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");

    let manifest = export(&env, &env.task, &bundle).unwrap();

    assert_eq!(manifest.patches.len(), 2);
    let names: Vec<_> = manifest.patches.iter().map(|p| p.file.clone()).collect();
    assert_eq!(names, vec![
        format!("patches/0001-{}.patch", Digest::of(comment_patch().as_bytes())),
        format!("patches/0002-{}.patch", Digest::of(fix_patch().as_bytes())),
    ]);
    assert_eq!(fs::read_to_string(bundle.join("patch.diff")).unwrap(), comment_patch() + &fix_patch());
    assert_eq!(Some(replay(&env, &bundle)), manifest.final_workspace_digest);
    assert_eq!(manifest.final_workspace_digest, Some(env.ws_digest()));
}

#[tokio::test]
async fn unfinished_tasks_are_refused_and_nothing_is_written() {
    let env = Env::new(10);
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");

    assert!(matches!(export(&env, &env.task, &bundle), Err(ExportError::NotTerminal(TaskState::Ready))));
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    assert!(matches!(export(&env, &env.task, &bundle), Err(ExportError::NotTerminal(TaskState::Running))));
    env.db.append(&env.task, &TaskEvent::Paused).unwrap();
    let err = export(&env, &env.task, &bundle).unwrap_err();
    assert!(matches!(err, ExportError::NotTerminal(TaskState::Paused)));
    assert_eq!(err.to_string(), "task is PAUSED, only finished tasks can be exported");

    assert!(entries(out_root.path()).is_empty(), "nothing written");
}

#[tokio::test]
async fn a_non_empty_destination_is_refused_and_an_empty_one_is_replaced() {
    let env = Env::new(10);
    succeeded(&env).await;
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join("keep.txt"), "mine").unwrap();

    assert!(matches!(export(&env, &env.task, &bundle), Err(ExportError::DestinationNotEmpty(_))));
    assert_eq!(entries(&bundle), vec!["keep.txt"], "existing content untouched");
    assert_eq!(entries(out_root.path()), vec!["bundle"]);

    fs::remove_file(bundle.join("keep.txt")).unwrap();
    export(&env, &env.task, &bundle).unwrap();
    assert!(bundle.join("manifest.json").is_file());
    assert_eq!(entries(out_root.path()), vec!["bundle"]);
}

#[tokio::test]
async fn a_missing_blob_mid_way_leaves_no_partial_bundle() {
    let env = Env::new(10);
    succeeded(&env).await;
    let evidence = env.effects("RunVerification")[0].result_digest.unwrap();
    fs::remove_file(blob_path(&env, &evidence)).unwrap();
    let out_root = tempfile::tempdir().unwrap();

    let err = export(&env, &env.task, &out_root.path().join("bundle")).unwrap_err();

    assert!(matches!(&err, ExportError::Io(e) if e.kind() == std::io::ErrorKind::NotFound), "{err:?}");
    assert!(entries(out_root.path()).is_empty(), "neither the bundle nor its temp directory remains");
}

#[tokio::test]
async fn a_corrupt_blob_fails_digest_reverification() {
    let env = Env::new(10);
    succeeded(&env).await;
    let evidence = env.effects("RunVerification")[0].result_digest.unwrap();
    let path = blob_path(&env, &evidence);
    let mut perms = fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&path, perms).unwrap();
    fs::write(&path, br#"{"passed": true}"#).unwrap();
    let out_root = tempfile::tempdir().unwrap();

    let err = export(&env, &env.task, &out_root.path().join("bundle")).unwrap_err();

    assert!(matches!(err, ExportError::Integrity(_)), "{err:?}");
    assert!(entries(out_root.path()).is_empty());
}

#[tokio::test]
async fn a_failed_task_is_exported_honestly() {
    let env = Env::new(10);
    let mut agent = FakeAgent::from_fixture_patch(comment_patch());
    assert_eq!(run(&env, &env.task, &mut agent).await, TaskState::Failed);
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");

    let manifest = export(&env, &env.task, &bundle).unwrap();

    assert_eq!(manifest.state, "FAILED");
    assert_eq!(manifest.verified_digest, None, "no success claim");
    assert_eq!(manifest.verification_results.len(), 1);
    assert!(manifest.verification_results.iter().all(|r| !r.passed));
    assert_ne!(manifest.verification_results[0].exit_code, Some(0));
    assert_eq!(fs::read_to_string(bundle.join("patch.diff")).unwrap(), comment_patch(), "the applied patch, as it was");
    assert_eq!(Some(replay(&env, &bundle)), manifest.final_workspace_digest);
}

#[tokio::test]
async fn a_task_cancelled_before_it_started_exports_an_empty_patch() {
    let env = Env::new(10);
    env.db.append(&env.task, &TaskEvent::CancelRequested).unwrap();
    env.db.append(&env.task, &TaskEvent::CancelCompleted).unwrap();
    let out_root = tempfile::tempdir().unwrap();
    let bundle = out_root.path().join("bundle");

    let manifest = export(&env, &env.task, &bundle).unwrap();

    assert_eq!(manifest.state, "CANCELLED");
    assert!(manifest.patches.is_empty());
    assert_eq!(fs::read(bundle.join("patch.diff")).unwrap(), b"");
    assert_eq!(manifest.patch_digest, Digest::of(b""));
    assert_eq!(manifest.base_workspace_digest, None, "no snapshot was ever read");
    assert_eq!(manifest.final_workspace_digest, None);
    assert_eq!(manifest.verified_digest, None);
    assert!(manifest.verification_results.is_empty());
    assert_eq!(manifest.verification_profile_digest, None);
}

/// A second task in `env` created the way the CLI does: digest of the serialized contract,
/// then a `Submitted` record of the resolved inputs.
fn submitted_task(env: &Env, repository: Digest, profile: Digest) -> TaskId {
    let digest = Digest::of(&serde_json::to_vec(&env.contract).unwrap());
    let task = env.db.create_task(&env.contract, &digest).unwrap();
    let payload = json!({
        "contract_digest": digest, "repository_digest": repository, "profile_id": "parser-checks-v1",
        "profile_digest": profile, "guest_image": "fixture-executor-v0", "model": "fake-agent",
    });
    env.db.append_audit(&task, "Submitted", &payload).unwrap();
    task
}

#[tokio::test]
async fn submitted_inputs_are_reported_and_cross_checked() {
    let env = Env::new(10);
    let (repo, profile) = (workspace_digest(&env.snapshot_dir()).unwrap(), workspace_digest(&env.profile_dir()).unwrap());
    let good = submitted_task(&env, repo, profile);
    let wrong_repo = submitted_task(&env, Digest::of(b"other repo"), profile);
    let wrong_profile = submitted_task(&env, repo, Digest::of(b"other profile"));
    for task in [&good, &wrong_repo, &wrong_profile] {
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        assert_eq!(run(&env, task, &mut agent).await, TaskState::Succeeded);
    }
    let out_root = tempfile::tempdir().unwrap();

    let manifest = export(&env, &good, &out_root.path().join("good")).unwrap();
    assert_eq!(manifest.base_revision, repo.to_string());
    assert_eq!(manifest.base_workspace_digest, Some(repo));
    assert_eq!(manifest.verification_profile_digest, Some(profile));
    assert_eq!(manifest.model.as_deref(), Some("fake-agent"));

    let err = export(&env, &wrong_repo, &out_root.path().join("repo")).unwrap_err();
    assert!(matches!(&err, ExportError::Inconsistent(m) if m.contains("snapshot")), "{err:?}");
    let err = export(&env, &wrong_profile, &out_root.path().join("profile")).unwrap_err();
    assert!(matches!(&err, ExportError::Inconsistent(m) if m.contains("profile")), "{err:?}");
    assert_eq!(entries(out_root.path()), vec!["good"]);

    // The contract digest of a submitted task is recomputed from the stored contract.
    let raw = env.task.clone();
    env.db.append_audit(&raw, "Submitted", &json!({ "repository_digest": repo, "profile_digest": profile })).unwrap();
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());
    assert_eq!(run(&env, &raw, &mut agent).await, TaskState::Succeeded);
    let err = export(&env, &raw, &out_root.path().join("raw")).unwrap_err();
    assert!(matches!(&err, ExportError::Inconsistent(m) if m.contains("contract")), "{err:?}");
}
