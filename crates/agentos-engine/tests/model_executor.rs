//! The model executor: one send per attempt, the response retained before it is returned,
//! and the routing executor that sends each effect kind to its executor.

mod common;

use std::path::Path;

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskEvent;
use agentos_engine::crash::{CrashHook, CrashPoint};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::model::executor::ModelExecutor;
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::{ModelProvider, ProviderResult, Usage};
use agentos_engine::supervised::ExecCounts;
use agentos_engine::workspace::list_files;
use common::{contract_model, routing_over, Env};

const GOOD: &[u8] = br#"{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;

fn contract() -> Contract {
    contract_model(3, 5).0
}

fn model_kind() -> EffectKind {
    EffectKind::ModelCall { model: "fake".into(), turn: 1 }
}

fn req_of(task: &TaskId, kind: EffectKind, payload: &[u8], contract: &Contract) -> EffectRequest {
    EffectRequest {
        effect_id: EffectId::derive(task, 0, &kind, &Digest::of(payload)),
        task_id: task.clone(),
        kind,
        payload: payload.to_vec(),
        contract: contract.clone(),
        deadline_ts: 0,
    }
}

fn req(kind: EffectKind) -> EffectRequest {
    req_of(&TaskId::new(), kind, b"{\"messages\":[]}", &contract())
}

fn ctx() -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "model".into() }
}

fn exec(root: &Path, results: Vec<ProviderResult>, counts: &ExecCounts) -> (ModelExecutor, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let provider = FakeProvider::scripted(results);
    let calls = provider.calls_counter();
    (ModelExecutor::new(root.to_path_buf(), Some(Box::new(provider)), counts.clone()), calls)
}

fn failure_reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_response_is_retained_before_it_is_returned_and_found_again() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(dir.path(), vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())], &counts);
    let (r, c) = (req(model_kind()), ctx());

    let out = ex.run(&r, &c).await;

    assert_eq!(out.receipt.outcome, Outcome::Success);
    assert_eq!(out.output, GOOD);
    assert_eq!(out.receipt.lease_generation, 1);
    assert!(!out.unresolved);
    let file = ex.retention_dir(&r.effect_id, &c.attempt_id).join("response.json");
    assert_eq!(dir.path().join(format!("{}-{}", r.effect_id, c.attempt_id)).join("response.json"), file);
    let kept: ExecOutcome = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(kept, out);
    let fresh = ModelExecutor::new(dir.path().to_path_buf(), None, counts.clone());
    assert_eq!(fresh.retained_outcome(&r.effect_id), Some(out));
    assert_eq!(counts.get("model_call"), 1);
}

#[tokio::test]
async fn a_rejected_status_is_a_settled_failure_that_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let rejected = ProviderResult::Rejected { status: 400, body: "bad\nrequest".into() };
    let (ex, _) = exec(dir.path(), vec![rejected], &counts);
    let r = req(model_kind());

    let out = ex.run(&r, &ctx()).await;

    assert_eq!(failure_reason(&out), "http 400: bad\\nrequest");
    assert!(!out.unresolved);
    assert_eq!(ex.retained_outcome(&r.effect_id), Some(out));
    assert_eq!(counts.get("model_call"), 1);
}

#[tokio::test]
async fn a_transport_failure_is_unresolved_and_not_retained() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(dir.path(), vec![ProviderResult::Transport("timed out".into())], &counts);
    let (r, c) = (req(model_kind()), ctx());

    let out = ex.run(&r, &c).await;

    assert!(out.unresolved, "{out:?}");
    assert_eq!(failure_reason(&out), "transport failure: timed out");
    assert!(!ex.retention_dir(&r.effect_id, &c.attempt_id).exists());
    assert_eq!(ex.retained_outcome(&r.effect_id), None);
    assert_eq!(counts.get("model_call"), 1);
}

#[tokio::test]
async fn a_malformed_body_is_a_settled_failure() {
    for body in [&b"not json"[..], &b"{\"stop_reason\":1}"[..], &b"{\"content\":[]}"[..], &b"[]"[..]] {
        let dir = tempfile::tempdir().unwrap();
        let (ex, _) = exec(dir.path(), vec![ProviderResult::Response(body.to_vec(), Usage::default())], &ExecCounts::default());
        let out = ex.run(&req(model_kind()), &ctx()).await;
        let reason = failure_reason(&out);
        assert!(reason.starts_with("malformed model response: "), "{reason}");
        assert!(!out.unresolved);
    }
}

#[tokio::test]
async fn no_provider_is_a_failure_without_a_call() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let ex = ModelExecutor::new(dir.path().to_path_buf(), None, counts.clone());

    let out = ex.run(&req(model_kind()), &ctx()).await;

    assert_eq!(failure_reason(&out), "no model provider configured");
    assert_eq!(counts.get("model_call"), 0);
}

#[tokio::test]
async fn another_kind_is_not_a_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let (ex, calls) = exec(dir.path(), vec![], &ExecCounts::default());
    let out = ex.run(&req(EffectKind::ReadSnapshot), &ctx()).await;
    assert_eq!(failure_reason(&out), "not a model call");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn past_the_deadline_nothing_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, calls) = exec(dir.path(), vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())], &counts);
    let mut r = req(model_kind());
    r.deadline_ts = 1;

    let out = ex.run(&r, &ctx()).await;

    assert_eq!(failure_reason(&out), "deadline exceeded");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(counts.get("model_call"), 0);
}

#[tokio::test]
async fn the_during_execute_hook_fires_after_the_send_and_before_retention() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let hook = CrashHook::at(CrashPoint::DuringExecute, "model_call");
    let (ex, calls) = exec(dir.path(), vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())], &counts);
    let ex = ex.with_crash(Some(hook.clone()));
    let r = req(model_kind());

    let out = ex.run(&r, &ctx()).await;

    assert!(failure_reason(&out).starts_with("injected crash"), "{out:?}");
    assert_eq!(hook.tripped(), Some(CrashPoint::DuringExecute));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(counts.get("model_call"), 1);
    assert_eq!(ex.retained_outcome(&r.effect_id), None);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing retained");
}

#[tokio::test]
async fn retained_outcome_ignores_other_effects_and_unresolved_files() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(dir.path(), vec![ProviderResult::Transport("x".into()), ProviderResult::Response(GOOD.to_vec(), Usage::default())], &counts);
    let other = req(model_kind());
    let mut mine = req(EffectKind::ModelCall { model: "fake".into(), turn: 2 });
    mine.effect_id = EffectId::derive(&mine.task_id, 1, &mine.kind, &Digest::of(b"mine"));
    let c = ctx();

    let unresolved = ex.run(&other, &c).await;
    assert!(unresolved.unresolved);
    // An unresolved outcome written by hand is still never returned.
    let d = ex.retention_dir(&other.effect_id, &c.attempt_id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("response.json"), serde_json::to_vec(&unresolved).unwrap()).unwrap();
    assert_eq!(ex.retained_outcome(&other.effect_id), None);

    let kept = ex.run(&mine, &ctx()).await;
    assert_eq!(ex.retained_outcome(&mine.effect_id), Some(kept));
    assert_eq!(ex.retained_outcome(&other.effect_id), None, "another effect's file is not mine");
}

#[tokio::test]
async fn the_highest_lease_generation_wins() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(dir.path(), vec![], &counts);
    let r = req(model_kind());
    let mut best = None;
    for generation in [2u64, 1, 3] {
        let c = AttemptCtx { attempt_id: AttemptId::new(), lease_generation: generation, worker: "m".into() };
        let out = ExecOutcome::success(&r, &c, GOOD.to_vec());
        let d = ex.retention_dir(&r.effect_id, &c.attempt_id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("response.json"), serde_json::to_vec(&out).unwrap()).unwrap();
        if generation == 3 {
            best = Some(out);
        }
    }
    assert_eq!(ex.retained_outcome(&r.effect_id), best);
}

#[tokio::test]
async fn routing_dispatches_by_kind_and_merges_retained_outcomes() {
    let env = Env::with_model(3, 5);
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let base = agentos_engine::workspace::workspace_digest(&env.snapshot_dir()).unwrap();
    env.db.append(&env.task, &TaskEvent::WorkspaceUpdated { digest: base }).unwrap();
    let counts = ExecCounts::default();
    let provider: Box<dyn ModelProvider> = Box::new(FakeProvider::scripted(vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())]));
    let routing = routing_over(env.dir.path(), env.fixture_exec(), Some(provider), &counts, None);

    let snap = req_of(&env.task, EffectKind::ReadSnapshot, b"", &env.contract);
    let out = routing.run(&snap, &ctx()).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{out:?}");
    assert_eq!(out.new_workspace, Some(base), "the fixture executor answers a snapshot read");

    let model = req_of(&env.task, model_kind(), b"{\"messages\":[]}", &env.contract);
    let out = routing.run(&model, &ctx()).await;
    assert_eq!(out.output, GOOD);
    assert_eq!(routing.retained_outcome(&model.effect_id), Some(out));
    assert_eq!(counts.get("model_call"), 1);
    assert_eq!(routing.retained_outcome(&snap.effect_id), None);

    let list = req_of(&env.task, EffectKind::ListFiles { turn: 1 }, b"", &env.contract);
    let out = routing.run(&list, &ctx()).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.output).unwrap();
    let want: Vec<String> = list_files(&env.snapshot_dir()).unwrap().into_iter().map(|(r, _)| r).collect();
    assert_eq!(v["files"], serde_json::json!(want));
    assert_eq!(out.new_workspace, None);
}
