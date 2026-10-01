mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::patch::patch_paths;
use agentos_engine::workspace::workspace_digest;
use common::{contract, copy_dir, create_patch, edit_patch, fix_patch, fixtures};
use tempfile::TempDir;

struct Fx {
    dir: TempDir,
    exec: FixtureExecutor,
    task: TaskId,
    contract: Contract,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let exec = FixtureExecutor::new(
            dir.path().join("snapshot"),
            dir.path().join("profile"),
            dir.path().join("work"),
        );
        Fx { dir, exec, task: TaskId::new(), contract: contract(10).0 }
    }

    fn ws(&self) -> std::path::PathBuf {
        self.exec.workspace(&self.task)
    }

    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        EffectRequest {
            effect_id: EffectId::derive(&self.task, 0, &kind, &Digest::of(payload)),
            task_id: self.task.clone(),
            kind,
            payload: payload.to_vec(),
            contract: self.contract.clone(),
        }
    }

    async fn run(&self, kind: EffectKind, payload: &[u8]) -> ExecOutcome {
        let ctx = AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "test".into() };
        let req = self.request(kind, payload);
        let out = self.exec.run(&req, &ctx).await;
        assert_eq!(out.receipt.effect_id, req.effect_id);
        assert_eq!(out.receipt.attempt_id, ctx.attempt_id);
        assert_eq!(out.receipt.lease_generation, 1);
        assert_eq!(out.receipt.result_digest, Some(Digest::of(&out.output)));
        out
    }

    async fn snapshot(&self) -> Digest {
        let out = self.run(EffectKind::ReadSnapshot, b"").await;
        assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
        out.new_workspace.unwrap()
    }

    async fn apply(&self, base: Digest, patch: &str) -> ExecOutcome {
        self.run(EffectKind::ApplyPatch { expected_base: base }, patch.as_bytes()).await
    }
}

fn reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!("expected failure, got success: {}", String::from_utf8_lossy(&out.output)),
    }
}

fn json(out: &ExecOutcome) -> serde_json::Value {
    serde_json::from_slice(&out.output).unwrap()
}

#[tokio::test]
async fn read_snapshot_copies_the_repo_and_reports_a_manifest() {
    let fx = Fx::new();
    fs::create_dir_all(fx.dir.path().join("snapshot/.git")).unwrap();
    fs::write(fx.dir.path().join("snapshot/.git/HEAD"), "x").unwrap();
    let digest = fx.snapshot().await;
    assert_eq!(digest, workspace_digest(&fx.dir.path().join("snapshot")).unwrap());
    assert_eq!(digest, workspace_digest(&fx.ws()).unwrap());
    assert!(!fx.ws().join(".git").exists());
    let out = fx.run(EffectKind::ReadSnapshot, b"").await;
    let manifest = json(&out);
    assert_eq!(manifest["workspace_digest"], digest.to_string());
    assert!(manifest["files"].as_array().unwrap().contains(&"src/parser.py".into()));

    // Re-running the snapshot resets the workspace (retry is idempotent).
    fs::write(fx.ws().join("src/extra.py"), "x").unwrap();
    assert_eq!(fx.snapshot().await, digest);
    assert!(!fx.ws().join("src/extra.py").exists());
}

#[tokio::test]
async fn apply_patch_changes_the_workspace_and_reports_paths() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let out = fx.apply(base, &fix_patch()).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
    let new = workspace_digest(&fx.ws()).unwrap();
    assert_eq!(out.new_workspace, Some(new));
    assert_ne!(new, base);
    let j = json(&out);
    assert_eq!(j["applied"], true);
    assert_eq!(j["paths"], serde_json::json!(["src/parser.py"]));
    assert_eq!(j["workspace_digest"], new.to_string());
}

// (c) executor level
#[tokio::test]
async fn apply_patch_with_a_stale_base_is_a_version_conflict_and_touches_nothing() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let out = fx.apply(Digest::of(b"stale"), &fix_patch()).await;
    assert!(reason(&out).contains("version conflict"), "{}", reason(&out));
    assert_eq!(out.new_workspace, None);
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), base);
}

// (f)
#[tokio::test]
async fn patch_through_a_symlinked_directory_is_rejected_without_writing_outside() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let outside = fx.dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fx.ws().join("src/escape")).unwrap();

    let out = fx.apply(base, &create_patch("src/escape/pwned.py", "owned")).await;
    let r = reason(&out);
    assert!(r.contains("symlink") && r.contains("src/escape/pwned.py"), "{r}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "nothing written outside the workspace");

    // An existing file reached through the link is equally refused.
    fs::write(outside.join("target.py"), "old\n").unwrap();
    let out = fx.apply(base, &edit_patch("src/escape/target.py", "old", "new")).await;
    assert!(reason(&out).contains("crosses symlink src/escape"), "{}", reason(&out));
    assert_eq!(fs::read_to_string(outside.join("target.py")).unwrap(), "old\n");
}

#[tokio::test]
async fn patch_onto_a_symlinked_file_is_rejected() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let outside = fx.dir.path().join("victim.py");
    fs::write(&outside, "old\n").unwrap();
    std::os::unix::fs::symlink(&outside, fx.ws().join("src/victim.py")).unwrap();
    let out = fx.apply(base, &edit_patch("src/victim.py", "old", "new")).await;
    assert!(reason(&out).contains("crosses symlink src/victim.py"), "{}", reason(&out));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "old\n");
}

#[tokio::test]
async fn parent_traversal_is_rejected_even_when_the_target_exists() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    // ws = work/<task>/ws, so ../victim.py resolves to work/<task>/victim.py.
    let victim = fx.ws().parent().unwrap().join("victim.py");
    fs::write(&victim, "old\n").unwrap();
    for patch in [
        edit_patch("../victim.py", "old", "new"),
        edit_patch("src/../../victim.py", "old", "new"),
    ] {
        let out = fx.apply(base, &patch).await;
        assert!(reason(&out).contains("not editable"), "{}", reason(&out));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "old\n");
    }
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), base);
}

#[tokio::test]
async fn executor_enforces_editable_paths_itself() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let out = fx.apply(base, &edit_patch("tests/test_parser.py", "import unittest", "import os")).await;
    assert!(reason(&out).contains("not editable"), "{}", reason(&out));
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), base);
}

#[tokio::test]
async fn rename_of_a_protected_file_into_src_is_rejected() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let rename = "diff --git a/tests/test_parser.py b/src/test_parser.py\nsimilarity index 100%\nrename from tests/test_parser.py\nrename to src/test_parser.py\n";
    let err = patch_paths(rename).await.unwrap_err();
    assert!(err.contains("rename"), "{err}");
    let out = fx.apply(base, rename).await;
    assert!(reason(&out).contains("rename"), "{}", reason(&out));
    assert!(fx.ws().join("tests/test_parser.py").exists());
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), base);
}

#[tokio::test]
async fn unsupported_patch_shapes_are_rejected() {
    let symlink = "diff --git a/src/l b/src/l\nnew file mode 120000\n--- /dev/null\n+++ b/src/l\n@@ -0,0 +1 @@\n+/etc\n\\ No newline at end of file\n";
    let mode = "diff --git a/src/parser.py b/src/parser.py\nold mode 100644\nnew mode 100755\n";
    let copy = "diff --git a/tests/test_parser.py b/src/t.py\nsimilarity index 100%\ncopy from tests/test_parser.py\ncopy to src/t.py\n";
    let binary = "diff --git a/src/b.bin b/src/b.bin\nnew file mode 100644\nindex 0000000..1111111\nGIT binary patch\nliteral 1\nIcmZpX000310RR91\n\nliteral 0\nHcmV?d00001\n\n";
    for (name, p) in [("symlink", symlink), ("mode", mode), ("copy", copy), ("binary", binary)] {
        assert!(patch_paths(p).await.is_err(), "{name} accepted");
    }
    assert!(patch_paths("").await.is_err());
    assert!(patch_paths("not a patch").await.is_err());
}

#[tokio::test]
async fn patch_paths_lists_created_deleted_and_quoted_paths() {
    let delete = "diff --git a/src/old.py b/src/old.py\ndeleted file mode 100644\n--- a/src/old.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
    let quoted = "diff --git \"a/src/we ird.py\" \"b/src/we ird.py\"\nnew file mode 100644\n--- /dev/null\n+++ \"b/src/we ird.py\"\n@@ -0,0 +1 @@\n+n\n";
    let both = format!("{}{delete}{quoted}", create_patch("src/new.py", "n"));
    assert_eq!(
        patch_paths(&both).await.unwrap(),
        vec!["src/new.py".to_string(), "src/old.py".into(), "src/we ird.py".into()]
    );
}

#[tokio::test]
async fn verification_reports_pass_and_fail_as_successful_effects() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let profile_digest = workspace_digest(&fx.dir.path().join("profile")).unwrap();

    let failing = fx.run(EffectKind::RunVerification, b"").await;
    assert_eq!(failing.receipt.outcome, Outcome::Success);
    let report = failing.verification.clone().unwrap();
    assert!(!report.passed);
    assert_eq!(report.workspace, base);
    let ev = json(&failing);
    assert_eq!(ev["exit_code"], 1);
    assert_eq!(ev["profile_digest"], profile_digest.to_string());
    assert_eq!(ev["workspace_digest"], base.to_string());
    assert!(ev["stdout"].as_str().unwrap().contains("FAIL"));

    let applied = fx.apply(base, &fix_patch()).await;
    let fixed = applied.new_workspace.unwrap();
    let passing = fx.run(EffectKind::RunVerification, b"").await;
    let report = passing.verification.clone().unwrap();
    assert!(report.passed);
    assert_eq!(report.workspace, fixed);
    assert_eq!(json(&passing)["exit_code"], 0);
    assert_eq!(workspace_digest(&fx.ws()).unwrap(), fixed, "verification leaves the workspace as is");
    assert!(!fx.ws().join("src/__pycache__").exists());
}

#[tokio::test]
async fn verification_timeout_is_an_effect_failure() {
    let fx = Fx::new();
    fs::write(
        fx.dir.path().join("profile/profile.json"),
        r#"{"id": "slow", "command": ["python3", "-c", "import time; time.sleep(30)"], "protected": true}"#,
    )
    .unwrap();
    let exec = FixtureExecutor::new(
        fx.dir.path().join("snapshot"),
        fx.dir.path().join("profile"),
        fx.dir.path().join("work"),
    )
    .with_verify_timeout(Duration::from_millis(300));
    let fx = Fx { exec, ..fx };
    fx.snapshot().await;
    let started = std::time::Instant::now();
    let out = fx.run(EffectKind::RunVerification, b"").await;
    assert_eq!(reason(&out), "timeout");
    assert!(out.verification.is_none());
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn verification_sees_only_path_from_the_environment() {
    let fx = Fx::new();
    fs::write(
        fx.dir.path().join("profile/profile.json"),
        r#"{"id": "env", "command": ["python3", "-c", "import json, os; print(json.dumps(sorted(os.environ)))"], "protected": true}"#,
    )
    .unwrap();
    fx.snapshot().await;
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some() || std::env::var_os("HOME").is_some());
    let out = fx.run(EffectKind::RunVerification, b"").await;
    let stdout = json(&out)["stdout"].as_str().unwrap().to_string();
    let keys: Vec<String> = serde_json::from_str(stdout.trim()).unwrap();
    // Python itself may add LC_CTYPE (locale coercion); nothing else leaks in.
    let allowed = ["LC_CTYPE", "PATH", "PYTHONDONTWRITEBYTECODE"];
    assert!(keys.iter().all(|k| allowed.contains(&k.as_str())), "{keys:?}");
    assert!(keys.contains(&"PATH".to_string()) && keys.contains(&"PYTHONDONTWRITEBYTECODE".to_string()));
}

#[tokio::test]
async fn export_is_not_implemented_by_the_executor() {
    let fx = Fx::new();
    let out = fx.run(EffectKind::ExportBundle, b"").await;
    assert_eq!(reason(&out), "not implemented in this milestone");
    assert_eq!(json(&out)["outcome"], "failure");
}

#[tokio::test]
async fn apply_without_a_snapshot_fails() {
    let fx = Fx::new();
    let out = fx.apply(Digest::of(b"x"), &fix_patch()).await;
    assert!(!reason(&out).is_empty());
    assert!(!Path::new(&fx.ws()).exists());
}
