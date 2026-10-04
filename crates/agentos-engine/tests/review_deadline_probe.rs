//! Review regression: a model request must stop at its task deadline.
mod common;

use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::{AttemptCtx, EffectRequest, Executor};
use agentos_engine::model::executor::ModelExecutor;
use agentos_engine::model::provider::{BoxFuture, ModelProvider, ProviderResult, Usage};
use agentos_engine::supervised::ExecCounts;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct SlowProvider;

impl ModelProvider for SlowProvider {
    fn complete<'a>(&'a self, _: &'a [u8]) -> BoxFuture<'a, ProviderResult> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            ProviderResult::Response(
                br#"{"content":[],"stop_reason":"end_turn"}"#.to_vec(),
                Usage::default(),
            )
        })
    }
}

#[tokio::test]
async fn model_call_stops_at_the_task_deadline() {
    let root = tempfile::tempdir().unwrap();
    let task = TaskId::new();
    let kind = EffectKind::ModelCall {
        model: "review-probe".into(),
        turn: 1,
    };
    let payload = b"{}".to_vec();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let request = EffectRequest {
        effect_id: EffectId::derive(&task, 0, &kind, &Digest::of(&payload)),
        task_id: task,
        kind,
        payload,
        contract: common::contract_model(3, 5).0,
        deadline_ts: now + 1,
    };
    let context = AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        worker: "review-probe".into(),
    };
    let executor = ModelExecutor::new(
        root.path().into(),
        Some(Box::new(SlowProvider)),
        ExecCounts::default(),
    );
    let started = Instant::now();
    let outcome = executor.run(&request, &context).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "task deadline exceeded: elapsed={:?}, outcome={:?}",
        started.elapsed(),
        outcome.receipt.outcome,
    );
    assert!(
        outcome.unresolved,
        "an interrupted sent request may have been billed"
    );
    assert_eq!(executor.retained_outcome(&request.effect_id), None);
}
