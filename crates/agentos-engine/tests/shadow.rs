//! The shadow reader: reads served from the snapshot plus the journaled patches, never from
//! a live workspace, and never outside the shadow.

mod common;

use std::fs;
use std::os::unix::fs::symlink;

use agentos_core::broker::Resource;
use agentos_core::budget::Reservation;
use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome, Receipt};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskEvent;
use agentos_engine::agent::{AgentAction, Observation};
use agentos_engine::crash::{CrashHook, CrashPoint};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::shadow::{check_path, read_from, ShadowReader, READ_LIMIT};
use agentos_engine::workspace::{list_files, workspace_digest};
use common::{fix_patch, Env};

const PATCHED: &str = "060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13";

fn reader(env: &Env) -> ShadowReader {
    ShadowReader::new(env.dir.path().join("agentos.db"), env.snapshot_dir(), env.dir.path().join("shadow"))
}

fn started(env: &Env) -> Digest {
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    env.db.append(&env.task, &TaskEvent::WorkspaceUpdated { digest: base }).unwrap();
    base
}

fn req_of(task: &TaskId, kind: EffectKind, contract: &Contract) -> EffectRequest {
    EffectRequest {
        effect_id: EffectId::derive(task, 0, &kind, &Digest::of(b"")),
        task_id: task.clone(),
        kind,
        payload: Vec::new(),
        contract: contract.clone(),
        deadline_ts: 0,
    }
}

fn ctx() -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "shadow".into() }
}

async fn read(env: &Env, r: &ShadowReader, path: &str) -> ExecOutcome {
    r.run(&req_of(&env.task, EffectKind::ReadFile { path: path.into(), turn: 1 }, &env.contract), &ctx()).await
}

fn reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        other => panic!("expected a failure, got {other:?}: {}", String::from_utf8_lossy(&out.output)),
    }
}

fn json(out: &ExecOutcome) -> serde_json::Value {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
    serde_json::from_slice(&out.output).unwrap()
}

/// Journals a completed ApplyPatch the way the runner does.
fn complete_patch(env: &Env, base: &Digest, patch: &str, patched: Digest) {
    let kind = EffectKind::ApplyPatch { expected_base: *base };
    let digest = Digest::of(patch.as_bytes());
    let rec = env
        .db
        .record_intent(&env.task, kind.clone(), digest, base, Reservation::for_kind(&kind, 0), &Resource::Paths(vec!["src/parser.py".into()]))
        .unwrap();
    let obs = Observation::Start { files: vec![], workspace: *base };
    env.db
        .append_audit(&env.task, "AgentTurn", &serde_json::json!({"turn": 1, "observation": obs, "action": AgentAction::ApplyPatch(patch.into())}))
        .unwrap();
    let attempt = AttemptId::new();
    env.db.mark_dispatched(&rec.effect_id, &attempt, "w", 1).unwrap();
    let result = serde_json::to_vec(&serde_json::json!({"workspace_digest": patched})).unwrap();
    let rd = env.blobs.put(&result).unwrap();
    env.db.register_artifact(&rd, result.len() as u64, "result", Some(&rec.effect_id), "{}").unwrap();
    let receipt = Receipt { effect_id: rec.effect_id.clone(), attempt_id: attempt, lease_generation: 1, outcome: Outcome::Success, result_digest: Some(rd) };
    env.db.complete_effect(&rec.effect_id, &receipt, Some(&rd), Some(TaskEvent::WorkspaceUpdated { digest: patched })).unwrap();
}

#[tokio::test]
async fn list_files_serves_the_snapshot_at_the_task_digest() {
    let env = Env::with_model(3, 5);
    let base = started(&env);
    let r = reader(&env);

    let out = r.run(&req_of(&env.task, EffectKind::ListFiles { turn: 1 }, &env.contract), &ctx()).await;

    let v = json(&out);
    let want: Vec<String> = list_files(&env.snapshot_dir()).unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(v["files"], serde_json::json!(want));
    assert_eq!(v["workspace_digest"], serde_json::json!(base));
    assert_eq!(out.new_workspace, None);
}

#[tokio::test]
async fn reads_see_the_patched_revision() {
    let env = Env::with_model(3, 5);
    let base = started(&env);
    complete_patch(&env, &base, &fix_patch(), Digest::from_hex(PATCHED).unwrap());
    let r = reader(&env);

    let out = read(&env, &r, "src/parser.py").await;

    let v = json(&out);
    assert!(v["content"].as_str().unwrap().contains("result[key.strip()] = value.strip()"), "{v}");
    assert_eq!(v["workspace_digest"], serde_json::json!(PATCHED));
    assert_eq!(v["truncated"], false);
    assert_eq!(v["path"], "src/parser.py");
    assert_eq!(out.new_workspace, None);
    assert!(!fs::read_to_string(env.snapshot_dir().join("src/parser.py")).unwrap().contains("result[key.strip()] = value.strip()"), "the snapshot is untouched");
}

#[tokio::test]
async fn a_tampered_shadow_is_rebuilt_and_a_journal_mismatch_is_rejected() {
    let env = Env::with_model(3, 5);
    let base = started(&env);
    complete_patch(&env, &base, &fix_patch(), Digest::from_hex(PATCHED).unwrap());
    let r = reader(&env);
    assert_eq!(json(&read(&env, &r, "src/parser.py").await)["workspace_digest"], serde_json::json!(PATCHED));

    fs::write(r.shadow_dir(&env.task).join("src/parser.py"), "garbage").unwrap();
    let again = json(&read(&env, &r, "src/parser.py").await);
    assert!(again["content"].as_str().unwrap().contains("result[key.strip()] = value.strip()"), "{again}");

    env.db.append(&env.task, &TaskEvent::WorkspaceUpdated { digest: Digest::of(b"elsewhere") }).unwrap();
    let bad = reason(&read(&env, &r, "src/parser.py").await);
    assert!(bad.starts_with("shadow workspace digest mismatch: expected "), "{bad}");
    assert!(bad.contains(", found "), "{bad}");
}

#[tokio::test]
async fn a_completed_patch_that_is_not_journaled_is_an_error_not_a_guess() {
    let env = Env::with_model(3, 5);
    let base = started(&env);
    // An intent and completion with no AgentTurn carrying the text.
    let kind = EffectKind::ApplyPatch { expected_base: base };
    let digest = Digest::of(b"x");
    let rec = env.db.record_intent(&env.task, kind.clone(), digest, &base, Reservation::for_kind(&kind, 0), &Resource::Paths(vec!["src/parser.py".into()])).unwrap();
    let attempt = AttemptId::new();
    env.db.mark_dispatched(&rec.effect_id, &attempt, "w", 1).unwrap();
    let result = b"{}".to_vec();
    let rd = env.blobs.put(&result).unwrap();
    env.db.register_artifact(&rd, 2, "result", Some(&rec.effect_id), "{}").unwrap();
    let receipt = Receipt { effect_id: rec.effect_id.clone(), attempt_id: attempt, lease_generation: 1, outcome: Outcome::Success, result_digest: Some(rd) };
    env.db.complete_effect(&rec.effect_id, &receipt, Some(&rd), Some(TaskEvent::WorkspaceUpdated { digest: Digest::of(b"y") })).unwrap();

    let out = read(&env, &reader(&env), "src/parser.py").await;

    assert!(reason(&out).contains(&format!("the patch of effect {} is not journaled", rec.effect_id)), "{}", reason(&out));
}

#[tokio::test]
async fn read_file_refuses_traversal_excluded_and_symlink_paths() {
    let env = Env::with_model(3, 5);
    started(&env);
    let r = reader(&env);
    let secret = env.dir.path().join("shadow").join("secret");
    fs::create_dir_all(secret.parent().unwrap()).unwrap();
    fs::write(&secret, "TOP-SECRET").unwrap();
    // `<shadow>/<task>/shadow/../secret` would be `<shadow>/<task>/secret`.
    let canary = env.dir.path().join("shadow").join(env.task.to_string()).join("secret");
    fs::create_dir_all(canary.parent().unwrap()).unwrap();
    fs::write(&canary, "TOP-SECRET").unwrap();

    for (path, why) in [
        ("../etc/passwd", "file not in the workspace: ../etc/passwd"),
        ("/etc/passwd", "file not in the workspace: /etc/passwd"),
        ("src/../../x", "file not in the workspace: src/../../x"),
        ("../secret", "file not in the workspace: ../secret"),
        ("", "file not in the workspace: "),
        ("src/__pycache__/x.pyc", "path excluded from the workspace digest: src/__pycache__/x.pyc"),
        ("src", "file not in the workspace: src"),
        ("src/nonexistent.py", "file not in the workspace: src/nonexistent.py"),
    ] {
        let out = read(&env, &r, path).await;
        assert_eq!(reason(&out), why);
        assert!(!String::from_utf8_lossy(&out.output).contains("TOP-SECRET"));
    }
    let nul = read(&env, &r, "src/a\0b").await;
    assert!(reason(&nul).starts_with("file not in the workspace: "), "{}", reason(&nul));
}

#[test]
fn a_symlink_on_the_path_is_refused_without_following_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    symlink("/etc", dir.path().join("src/link")).unwrap();
    symlink("/etc/passwd", dir.path().join("src/file")).unwrap();
    assert_eq!(read_from(dir.path(), "src/link/passwd").unwrap_err(), "path src/link/passwd crosses symlink src/link");
    assert_eq!(read_from(dir.path(), "src/file").unwrap_err(), "path src/file crosses symlink src/file");
    assert_eq!(read_from(dir.path(), "src").unwrap_err(), "file not in the workspace: src");
}

#[tokio::test]
async fn a_symlink_in_the_snapshot_never_reaches_a_read() {
    let env = Env::with_model(3, 5);
    started(&env);
    symlink("/etc", env.snapshot_dir().join("src/link")).unwrap();
    let out = read(&env, &reader(&env), "src/link/passwd").await;
    assert!(!reason(&out).is_empty());
    assert!(!String::from_utf8_lossy(&out.output).contains("root:"));
}

#[tokio::test]
async fn reads_are_capped_at_64_kib_with_the_truncated_flag() {
    let env = Env::with_model(3, 5);
    fs::write(env.snapshot_dir().join("big.txt"), "a".repeat(70_000)).unwrap();
    fs::write(env.snapshot_dir().join("exact.txt"), "b".repeat(READ_LIMIT)).unwrap();
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    env.db.append(&env.task, &TaskEvent::WorkspaceUpdated { digest: base }).unwrap();
    let r = reader(&env);

    let big = json(&read(&env, &r, "big.txt").await);
    assert_eq!(big["content"].as_str().unwrap().len(), 65_536);
    assert_eq!(big["truncated"], true);
    let exact = json(&read(&env, &r, "exact.txt").await);
    assert_eq!(exact["content"].as_str().unwrap().len(), 65_536);
    assert_eq!(exact["truncated"], false);
}

#[test]
fn check_path_table() {
    assert_eq!(check_path("src/parser.py"), Ok(()));
    assert_eq!(check_path("a/b/c.txt"), Ok(()));
    assert_eq!(check_path(""), Err("file not in the workspace: ".into()));
    assert_eq!(check_path("/x"), Err("file not in the workspace: /x".into()));
    assert_eq!(check_path("a/../b"), Err("file not in the workspace: a/../b".into()));
    assert_eq!(check_path(".."), Err("file not in the workspace: ..".into()));
    assert!(check_path("a\0b").unwrap_err().starts_with("file not in the workspace: "));
    assert_eq!(check_path(".git/config"), Err("path excluded from the workspace digest: .git/config".into()));
    assert_eq!(check_path("x/m.pyc"), Err("path excluded from the workspace digest: x/m.pyc".into()));
    for p in ["./x", "a//b", "a/./b", "a/", "."] {
        assert_eq!(check_path(p), Err(format!("file not in the workspace: {p}")), "{p:?}");
    }
    let hostile = format!("./\nINJECTED\u{1b}[2J{}", "é".repeat(5000));
    let e = check_path(&hostile).unwrap_err();
    assert!(e.starts_with("file not in the workspace: ./\\nINJECTED"), "{e}");
    assert!(e.len() < 1000 && !e.chars().any(|c| c.is_control()), "bounded and escaped");
    let long = format!("a\n{}", "é".repeat(5000));
    let e = check_path(&format!("../{long}")).unwrap_err();
    assert!(!e.contains('\n') && e.len() < 1000, "model text is escaped and bounded");
}

#[tokio::test]
async fn the_during_execute_hook_fires_after_the_read() {
    let env = Env::with_model(3, 5);
    started(&env);
    let hook = CrashHook::at(CrashPoint::DuringExecute, "read_file");
    let r = reader(&env).with_crash(Some(hook.clone()));

    let out = read(&env, &r, "src/parser.py").await;

    assert_eq!(reason(&out), "injected crash after the read");
    assert_eq!(hook.tripped(), Some(CrashPoint::DuringExecute));
}

#[tokio::test]
async fn another_kind_is_not_a_read_effect() {
    let env = Env::with_model(3, 5);
    started(&env);
    let out = reader(&env).run(&req_of(&env.task, EffectKind::RunVerification, &env.contract), &ctx()).await;
    assert_eq!(reason(&out), "not a read effect");
}

#[test]
fn read_from_never_leaks_raw_errors_for_dotted_paths() {
    let dir = tempfile::tempdir().unwrap();
    for p in ["./x", "./\nINJECTED", "a//b", "src/"] {
        let e = read_from(dir.path(), p).unwrap_err();
        assert!(e.starts_with("file not in the workspace: "), "{e}");
        assert!(!e.chars().any(|c| c.is_control()) && e.len() < 1000, "{e}");
    }
}
