//! Recovery of a lost model call: it is forfeited (FAILED, reservation uncertain) and the
//! agent asks again; the task goes on.

mod common;

use agentos_core::broker::Resource;
use agentos_core::budget::Reservation;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::Digest;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::recover::{recover, Decision};
use common::Env;

/// An executor that never ran anything and kept nothing.
struct NoReceipt;

impl Executor for NoReceipt {
    async fn run(&self, _req: &EffectRequest, _ctx: &AttemptCtx) -> ExecOutcome {
        panic!("recovery must not run a model call again")
    }
}

/// An executor that retained a response for the dispatched attempt.
struct Retained {
    attempt: AttemptId,
    contract: agentos_core::contract::Contract,
    rec: EffectRecord,
}

impl Executor for Retained {
    async fn run(&self, _req: &EffectRequest, _ctx: &AttemptCtx) -> ExecOutcome {
        panic!("recovery must not run a model call again")
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        assert_eq!(*effect, self.rec.effect_id);
        let req = EffectRequest {
            effect_id: self.rec.effect_id.clone(),
            task_id: self.rec.task_id.clone(),
            kind: self.rec.kind.clone(),
            payload: Vec::new(),
            contract: self.contract.clone(),
            deadline_ts: 0,
        };
        let ctx = AttemptCtx { attempt_id: self.attempt.clone(), lease_generation: 1, worker: "w".into() };
        let body = b"{\"content\":[],\"stop_reason\":\"end_turn\",\"usage\":{\"output_tokens\":1}}";
        Some(ExecOutcome::success(&req, &ctx, body.to_vec()))
    }
}

/// An executor whose attempt cannot be decided.
struct Unresolved;

impl Executor for Unresolved {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        ExecOutcome::unresolved(req, ctx, "transport failure: timed out")
    }
}

fn intend_model_call(env: &Env, body: &[u8]) -> EffectRecord {
    let kind = EffectKind::ModelCall { model: "fake".into(), turn: 1 };
    let base = env.db.task(&env.task).unwrap().workspace_digest;
    let reserve = Reservation::for_kind(&kind, 1);
    env.db.record_intent(&env.task, kind, Digest::of(body), &base, reserve, &Resource::Task).unwrap()
}

fn dispatched_model_call(env: &Env) -> (EffectRecord, AttemptId) {
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let rec = intend_model_call(env, b"body");
    let attempt = AttemptId::new();
    env.db.mark_dispatched(&rec.effect_id, &attempt, "w", 1).unwrap();
    (rec, attempt)
}

#[tokio::test]
async fn a_dispatched_model_call_without_a_receipt_is_forfeited_not_unreconcilable() {
    let env = Env::with_model(3, 5);
    let (rec, _) = dispatched_model_call(&env);

    let report = recover(&env.db, &env.blobs, &NoReceipt, &env.task).await.unwrap();

    assert_eq!(report.decisions.len(), 1, "{report:?}");
    let d = &report.decisions[0];
    assert_eq!(d.decision, Decision::Forfeit);
    assert_eq!(d.kind, "model_call");
    assert_eq!(d.found_state, EffectState::Dispatched);
    assert_eq!(d.lease_generation, 1);
    let after = env.db.effect(&rec.effect_id).unwrap();
    assert_eq!((after.state, after.result_digest), (EffectState::Failed, None));
    assert_eq!(env.db.usage_summary(&env.task).unwrap().uncertain_model_requests, 1);
    assert_eq!(env.db.task(&env.task).unwrap().state, TaskState::Running, "the task goes on");
    assert_eq!(report.state, Some(TaskState::Running));
    assert_eq!(env.count("EffectForfeited"), 1);
    assert_eq!(env.count("EffectUnknown"), 0);

    let n = env.events().len();
    let again = recover(&env.db, &env.blobs, &NoReceipt, &env.task).await.unwrap();
    assert!(again.decisions.is_empty(), "{again:?}");
    assert_eq!(env.events().len(), n, "recovering again journals nothing");
}

#[tokio::test]
async fn an_unknown_model_call_is_forfeited_on_a_terminal_task_too() {
    let env = Env::with_model(3, 5);
    let (rec, _) = dispatched_model_call(&env);
    env.db.mark_unknown(&rec.effect_id).unwrap();
    env.db.append(&env.task, &TaskEvent::Failed { reason: "something else".into() }).unwrap();

    let report = recover(&env.db, &env.blobs, &NoReceipt, &env.task).await.unwrap();

    assert_eq!(report.decisions.len(), 1, "{report:?}");
    assert_eq!(report.decisions[0].decision, Decision::Forfeit);
    assert_eq!(report.decisions[0].found_state, EffectState::Unknown);
    assert_eq!(env.db.effect(&rec.effect_id).unwrap().state, EffectState::Failed);
    assert_eq!(env.db.usage_summary(&env.task).unwrap().uncertain_model_requests, 1);
    assert!(report.abandoned.is_empty(), "{report:?}");
    assert_eq!(env.db.task(&env.task).unwrap().state, TaskState::Failed);
}

#[tokio::test]
async fn a_retained_model_response_is_published_not_forfeited() {
    let env = Env::with_model(3, 5);
    let (rec, attempt) = dispatched_model_call(&env);
    let exec = Retained { attempt, contract: env.contract.clone(), rec: rec.clone() };

    let report = recover(&env.db, &env.blobs, &exec, &env.task).await.unwrap();

    assert_eq!(report.decisions.len(), 1, "{report:?}");
    assert_eq!(report.decisions[0].decision, Decision::PublishRetained);
    assert_eq!(env.db.effect(&rec.effect_id).unwrap().state, EffectState::Completed);
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!((usage.settled_model_requests, usage.uncertain_model_requests), (1, 0));
    assert_eq!(env.count("EffectForfeited"), 0);
}

#[tokio::test]
async fn run_attempt_forfeits_an_unresolved_model_call() {
    let env = Env::with_model(3, 5);
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    env.blobs.put(b"body").unwrap();
    let rec = intend_model_call(&env, b"body");
    // Registered, or recovery's blob collection would remove it.
    env.db.register_artifact(&rec.request_digest, 4, "model-request", Some(&rec.effect_id), "{}").unwrap();
    assert_eq!(rec.state, EffectState::Intended);

    let report = recover(&env.db, &env.blobs, &Unresolved, &env.task).await.unwrap();

    let decisions: Vec<Decision> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::Dispatch], "{report:?}");
    let after = env.db.effect(&rec.effect_id).unwrap();
    assert_eq!((after.state, after.result_digest), (EffectState::Failed, None));
    assert_eq!(env.db.usage_summary(&env.task).unwrap().uncertain_model_requests, 1);
    assert_eq!(env.db.task(&env.task).unwrap().state, TaskState::Running);
    let forfeited: Vec<_> = env.events().into_iter().filter(|e| e.event_type == "EffectForfeited").collect();
    assert_eq!(forfeited.len(), 1);
    assert_eq!(forfeited[0].payload["reason"], "transport failure: timed out");
    assert_eq!(forfeited[0].payload["lease_generation"], 1);
}

#[tokio::test]
async fn an_intended_model_call_whose_body_is_gone_is_unreconcilable() {
    let env = Env::with_model(3, 5);
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let rec = intend_model_call(&env, b"body");

    let report = recover(&env.db, &env.blobs, &NoReceipt, &env.task).await.unwrap();

    let decisions: Vec<Decision> = report.decisions.iter().map(|d| d.decision).collect();
    assert_eq!(decisions, vec![Decision::Unreconcilable], "{report:?}");
    assert_eq!(report.state, Some(TaskState::Failed));
    assert_ne!(env.db.effect(&rec.effect_id).unwrap().state, EffectState::Failed);
}
