//! The runner performs model calls, file listings and file reads as journaled effects: the
//! scripted end-to-end flow, replay and resume of model sessions, provider-error paths and
//! the gates that live in the runner.

mod common;

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use agentos_core::effect::{EffectKind, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::model::fake::{FakeProvider, load_transcript};
use agentos_engine::model::provider::{ProviderResult, usage_of};
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::runner::{run_task, run_task_with};
use agentos_engine::workspace::workspace_digest;
use agentos_store::db::Db;
use common::{ALL_CAPS, Env, flow, flow_with, model_agent, run_model, transcript};
use serde_json::{Value, json};

fn failed_reason(env: &Env) -> String {
    let failed = env
        .events()
        .into_iter()
        .rev()
        .find(|e| e.event_type == "Failed")
        .expect("a Failed event");
    failed.payload["Failed"]["reason"]
        .as_str()
        .unwrap()
        .to_string()
}

fn policy_one(env: &Env) {
    env.db
        .append_audit(
            &env.task,
            "Submitted",
            &json!({
                "model_policy_version": 1, "model_limits_version": 1, "model": "fake:test"
            }),
        )
        .unwrap();
}

#[tokio::test]
async fn policy_one_transport_loss_waits_and_preserves_uncertain_usage() {
    let mut responses = vec![ProviderResult::Transport("lost".into())];
    responses.extend(fixture_responses("parser-fix"));
    let (env, exec, provider) = flow_with(FakeProvider::scripted(responses), 12, 10);
    policy_one(&env);
    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
    assert_eq!(provider.calls(), 7);
    assert_eq!(env.count("ModelRetryScheduled"), 1);
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(
        (
            usage.uncertain_model_requests,
            usage.settled_model_requests,
            usage.reserved_model_requests
        ),
        (1, 6, 0)
    );
}

#[tokio::test]
async fn policy_one_malformed_complete_response_stops_after_one_send() {
    let (env, exec, provider) = flow_with(
        FakeProvider::scripted(vec![ProviderResult::Response(
            b"not JSON".to_vec(),
            Default::default(),
        )]),
        12,
        10,
    );
    policy_one(&env);
    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);
    assert_eq!(provider.calls(), 1);
    assert!(failed_reason(&env).contains("malformed model response"));
}

#[tokio::test]
async fn policy_one_stops_permanent_errors_after_one_settled_send() {
    for status in [400, 401, 403, 404, 302] {
        let (env, exec, provider) = flow_with(
            FakeProvider::scripted(vec![ProviderResult::Rejected {
                status,
                body: "denied".into(),
            }]),
            12,
            10,
        );
        policy_one(&env);
        assert_eq!(run_model(&env, &exec).await, TaskState::Failed);
        assert_eq!(provider.calls(), 1, "status {status}");
        assert!(
            failed_reason(&env).contains(&format!("http {status}")),
            "{}",
            failed_reason(&env)
        );
        assert_eq!(
            env.db
                .usage_summary(&env.task)
                .unwrap()
                .settled_model_requests,
            1
        );
    }
}

#[tokio::test]
async fn policy_one_records_a_wait_then_uses_a_fresh_effect() {
    let mut responses = vec![ProviderResult::Rejected {
        status: 529,
        body: "busy".into(),
    }];
    responses.extend(fixture_responses("parser-fix"));
    let (env, exec, provider) = flow_with(FakeProvider::scripted(responses), 12, 10);
    policy_one(&env);
    let started = std::time::Instant::now();
    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(provider.calls(), 7);
    assert_eq!(env.count("ModelRetryScheduled"), 1);
    let calls = env.effects("ModelCall");
    assert_ne!(calls[0].effect_id, calls[1].effect_id);
    assert_eq!(calls[0].request_digest, calls[1].request_digest);
    assert_eq!(
        env.db
            .usage_summary(&env.task)
            .unwrap()
            .settled_model_requests,
        7
    );
}

#[test]
fn legacy_failure_observation_serializes_without_new_metadata() {
    let fixture = json!({"ModelCallFailed": {"reason": "http 401: denied"}});
    let observation: agentos_engine::agent::Observation =
        serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(serde_json::to_value(observation).unwrap(), fixture);
    let new_fixture = json!({"ModelCallFailed": {"reason": "http 429: busy",
        "failure": {"class": "Transient", "retry_not_before_ts": 120}}});
    let observation: agentos_engine::agent::Observation =
        serde_json::from_value(new_fixture.clone()).unwrap();
    assert_eq!(serde_json::to_value(observation).unwrap(), new_fixture);
}

#[tokio::test]
async fn retry_schedule_survives_crashes_without_an_extra_reservation() {
    for (point, kind, occurrence) in [
        (CrashPoint::AfterComplete, "model_retry", 0),
        (CrashPoint::AfterAgentTurnJournaled, "model_call", 1),
        (CrashPoint::AfterIntent, "model_call", 1),
    ] {
        let mut responses = vec![ProviderResult::Rejected {
            status: 429,
            body: "busy".into(),
        }];
        responses.extend(fixture_responses("parser-fix"));
        let (env, exec, provider) = flow_with(FakeProvider::scripted(responses), 12, 10);
        policy_one(&env);
        let opts = RunOptions::crash_with(CrashHook::new(move |p, ctx| {
            p == point && ctx.kind == Some(kind) && ctx.occurrence == occurrence
        }));
        let error = run_task_with(
            &env.db,
            &env.blobs,
            &exec,
            &mut model_agent(&env),
            &env.task,
            &opts,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            agentos_engine::runner::EngineError::Crashed(_)
        ));
        assert_eq!(provider.calls(), 1);
        let original = env
            .events()
            .into_iter()
            .find(|e| e.event_type == "ModelRetryScheduled")
            .unwrap()
            .payload;
        assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
        assert_eq!(provider.calls(), 7);
        assert_eq!(env.count("ModelRetryScheduled"), 1);
        assert_eq!(
            env.events()
                .into_iter()
                .find(|e| e.event_type == "ModelRetryScheduled")
                .unwrap()
                .payload,
            original
        );
        assert_eq!(env.effects("ModelCall").len(), 7);
        assert_eq!(
            env.db
                .usage_summary(&env.task)
                .unwrap()
                .settled_model_requests,
            7
        );
    }
}

#[tokio::test]
async fn retry_wait_obeys_pause_cancel_and_revocation_without_more_sends() {
    for action in ["pause", "cancel", "revoke"] {
        let (env, exec, _) = flow_with(FakeProvider::scripted(vec![]), 12, 10);
        policy_one(&env);
        // Keep the provider timestamp within the deadline, so the test actually waits.
        let deadline = env.db.deadline_ts(&env.task).unwrap();
        // Replace the script with a wait of up to 30 seconds.
        let waiting = FakeProvider::scripted(vec![ProviderResult::RejectedWithRetryAfter {
            status: 429,
            body: "busy".into(),
            retry_not_before_ts: deadline - 1,
        }]);
        let exec = RoutingExecutor::new(
            exec.jobs,
            agentos_engine::model::ModelExecutor::new(
                env.dir.path().join("model"),
                Some(Box::new(waiting.clone())),
                Default::default(),
            ),
            exec.reads,
        );
        let writer = env.second_db();
        let interrupt = async {
            for _ in 0..100 {
                if writer
                    .events(&env.task)
                    .unwrap()
                    .iter()
                    .any(|e| e.event_type == "ModelRetryScheduled")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                writer
                    .events(&env.task)
                    .unwrap()
                    .iter()
                    .any(|e| e.event_type == "ModelRetryScheduled")
            );
            let instant = std::time::Instant::now();
            match action {
                "pause" => {
                    writer.append(&env.task, &TaskEvent::Paused).unwrap();
                }
                "cancel" => {
                    writer
                        .append(&env.task, &TaskEvent::CancelRequested)
                        .unwrap();
                }
                _ => {
                    writer
                        .revoke(
                            &env.task,
                            Some(agentos_core::contract::Capability::ModelRequest),
                        )
                        .unwrap();
                }
            }
            instant
        };
        let (state, interrupted_at) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(run_model(&env, &exec), interrupt)
            })
            .await
            .unwrap();
        assert_eq!(
            state,
            match action {
                "pause" => TaskState::Paused,
                "cancel" => TaskState::Cancelled,
                _ => TaskState::Failed,
            }
        );
        assert!(interrupted_at.elapsed() < std::time::Duration::from_millis(500));
        assert_eq!(waiting.calls(), 1);
        assert_eq!(env.effects("ModelCall").len(), 1);
        assert_eq!(
            env.db
                .usage_summary(&env.task)
                .unwrap()
                .reserved_model_requests,
            0
        );
    }
}

#[tokio::test]
async fn retry_after_the_deadline_fails_without_a_second_send() {
    let (env, exec, provider) = flow_with(
        FakeProvider::scripted(vec![ProviderResult::RejectedWithRetryAfter {
            status: 429,
            body: "busy".into(),
            retry_not_before_ts: i64::MAX,
        }]),
        12,
        10,
    );
    policy_one(&env);
    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);
    assert_eq!(provider.calls(), 1);
    assert!(failed_reason(&env).contains("retry would exceed task deadline"));
}

#[tokio::test]
async fn policy_one_request_limit_precedes_turn_blob_and_reservation() {
    for size in [8 * 1024 * 1024, 8 * 1024 * 1024 + 1] {
        let (env, exec, provider) = flow_with(
            FakeProvider::scripted(vec![ok(json!({
                "content": [], "stop_reason": "end_turn"
            }))]),
            12,
            10,
        );
        policy_one(&env);
        let body = vec![b' '; size];
        let request = Digest::of(&body);
        let mut agent = agentos_engine::agent::FakeAgent::scripted(vec![
            agentos_engine::agent::AgentAction::CallModel { request, body },
        ]);
        assert_eq!(
            run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
                .await
                .unwrap(),
            TaskState::Failed
        );
        if size == 8 * 1024 * 1024 {
            assert_eq!(provider.calls(), 1);
            assert_eq!(env.effects("ModelCall").len(), 1);
        } else {
            assert_eq!(provider.calls(), 0);
            assert!(env.effects("ModelCall").is_empty());
            assert_eq!(env.count("AgentTurn"), 0);
            assert!(!env.blobs.exists(&request));
            assert!(failed_reason(&env).contains("context size"));
        }
    }
}

fn ok(v: Value) -> ProviderResult {
    ProviderResult::Response(serde_json::to_vec(&v).unwrap(), usage_of(&v))
}

fn tool_use(n: u32, name: &str, input: Value) -> ProviderResult {
    ok(json!({
        "id": format!("msg_{n}"), "type": "message", "role": "assistant", "model": "fake",
        "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5},
        "content": [{"type": "tool_use", "id": format!("toolu_{n}"), "name": name, "input": input}],
    }))
}

fn fixture_responses(name: &str) -> Vec<ProviderResult> {
    load_transcript(&transcript(name))
        .unwrap()
        .responses
        .into_iter()
        .map(|e| {
            ProviderResult::Response(
                serde_json::to_vec(&e.response).unwrap(),
                usage_of(&e.response),
            )
        })
        .collect()
}

/// Model-call effects, with the turn each was intended on.
fn model_turns(env: &Env) -> Vec<u32> {
    env.effects("ModelCall")
        .iter()
        .map(|r| match r.kind {
            EffectKind::ModelCall { turn, .. } => turn,
            _ => unreachable!(),
        })
        .collect()
}

fn agent_turn_observations(env: &Env) -> Vec<Value> {
    env.events()
        .into_iter()
        .filter(|e| e.event_type == "AgentTurn")
        .map(|e| e.payload["observation"].clone())
        .collect()
}

#[tokio::test]
async fn the_scripted_transcript_fixes_the_fixture_through_the_broker() {
    let (env, exec, provider) = flow("parser-fix", 12, 10);

    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);

    let task = env.db.task(&env.task).unwrap();
    assert_eq!(task.verified_digest, Some(task.workspace_digest));
    assert!(env.ws().join("src/notes.txt").exists());
    assert_eq!(workspace_digest(&env.ws()).unwrap(), task.workspace_digest);
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(usage.settled_model_requests, 6);
    assert_eq!(usage.uncertain_model_requests, 0);
    assert_eq!(usage.reserved_model_requests, 0);
    assert_eq!(usage.settled_tool_actions, 5);

    let calls = env.effects("ModelCall");
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|r| r.state == EffectState::Completed && r.lease_generation == 1),
        "{calls:?}"
    );
    assert_eq!(env.effects("ListFiles").len(), 1);
    assert_eq!(env.effects("ReadFile").len(), 1);
    assert_eq!(env.effects("ApplyPatch").len(), 2);
    assert_eq!(env.effects("RunVerification").len(), 2);
    assert_eq!(
        (env.count("VerifyFailed"), env.count("VerifyPassed")),
        (1, 1)
    );
    assert_eq!(provider.calls(), 6);

    // Each model call: its turn, the intent, the request artifact linked, the dispatch, the
    // response artifact, the completion; nothing else, and no workspace update.
    let events = env.events();
    let turn_at: Vec<usize> = (0..events.len())
        .filter(|&i| events[i].event_type == "AgentTurn")
        .collect();
    let mut seen = 0;
    for (n, &start) in turn_at.iter().enumerate() {
        if events[start].payload["action"].get("CallModel").is_none() {
            continue;
        }
        seen += 1;
        let end = turn_at.get(n + 1).copied().unwrap_or(events.len());
        // The broker journals its grants (`CapabilityGranted`) around the intent and the dispatch.
        let segment: Vec<&Value> = events[start..end]
            .iter()
            .filter(|e| e.event_type != "CapabilityGranted")
            .map(|e| &e.payload)
            .collect();
        let types: Vec<&str> = events[start..end]
            .iter()
            .map(|e| e.event_type.as_str())
            .filter(|t| *t != "CapabilityGranted")
            .collect();
        assert_eq!(
            types,
            [
                "AgentTurn",
                "EffectIntended",
                "ArtifactRegistered",
                "EffectDispatched",
                "ArtifactRegistered",
                "EffectCompleted"
            ]
        );
        assert_eq!(segment[2]["type"], "model-request");
        assert_eq!(segment[4]["type"], "model-response");
    }
    assert_eq!(seen, 6);

    // The result blob of each is the fixture response, byte for byte.
    let fixture = load_transcript(&transcript("parser-fix")).unwrap();
    for (rec, entry) in calls.iter().zip(&fixture.responses) {
        assert_eq!(
            env.blobs.get(&rec.result_digest.unwrap()).unwrap(),
            serde_json::to_vec(&entry.response).unwrap()
        );
        assert!(
            env.blobs.exists(&rec.request_digest),
            "the request body is stored"
        );
    }
    assert_eq!(env.count("Denied"), 0);
    assert_eq!(env.count("EffectForfeited"), 0);
}

#[tokio::test]
async fn the_request_body_is_registered_before_its_turn_is_journaled() {
    let (env, exec, provider) = flow("parser-fix", 12, 10);
    let mut agent = model_agent(&env);
    let opts = RunOptions::crash_with(CrashHook::at(
        CrashPoint::AfterAgentTurnJournaled,
        "model_call",
    ));

    let err = run_task_with(&env.db, &env.blobs, &exec, &mut agent, &env.task, &opts)
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            agentos_engine::runner::EngineError::Crashed(CrashPoint::AfterAgentTurnJournaled)
        ),
        "{err:?}"
    );
    assert_eq!(env.effects("ModelCall").len(), 0, "no intent yet");
    let turn = env
        .events()
        .into_iter()
        .rev()
        .find(|e| e.event_type == "AgentTurn")
        .unwrap();
    let request: Digest =
        serde_json::from_value(turn.payload["action"]["CallModel"]["request"].clone()).unwrap();
    assert!(env.blobs.exists(&request));
    assert!(
        env.db.referenced_blobs().unwrap().contains(&request),
        "registered, so recovery's gc keeps it"
    );
    assert_eq!(provider.calls(), 0);

    // A restart replays the model turn (journaled with an empty body) without divergence and
    // goes on to the end.
    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
    assert_eq!(provider.calls(), 6);
    assert!(
        env.effects("ModelCall")
            .iter()
            .all(|r| r.lease_generation == 1)
    );
}

#[tokio::test]
async fn a_crash_after_the_intent_resumes_the_same_call_without_a_second_send() {
    let (env, exec, provider) = flow("parser-fix", 12, 10);
    let mut agent = model_agent(&env);
    let opts = RunOptions::crash_with(CrashHook::new(|p, ctx| {
        p == CrashPoint::AfterIntent && ctx.kind == Some("model_call") && ctx.occurrence == 2
    }));

    let err = run_task_with(&env.db, &env.blobs, &exec, &mut agent, &env.task, &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            agentos_engine::runner::EngineError::Crashed(CrashPoint::AfterIntent)
        ),
        "{err:?}"
    );
    assert_eq!(provider.calls(), 2);
    assert_eq!(env.effects("ModelCall").len(), 3);

    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);

    // Recovery kept the unlinked request blob alive and sent the interrupted call once.
    assert_eq!(provider.calls(), 6);
    let calls = env.effects("ModelCall");
    assert_eq!(calls.len(), 6);
    assert!(
        calls
            .iter()
            .all(|r| r.state == EffectState::Completed && r.lease_generation == 1),
        "{calls:?}"
    );
    assert_eq!(env.count("EffectForfeited"), 0);
}

/// Appends `Paused` from a second connection the first time a file is read.
struct PauseOnFirstRead {
    inner: RoutingExecutor<FixtureExecutor>,
    writer: Mutex<Db>,
    task: TaskId,
    done: AtomicBool,
}

impl Executor for PauseOnFirstRead {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        if matches!(req.kind, EffectKind::ReadFile { .. })
            && !self.done.swap(true, Ordering::SeqCst)
        {
            self.writer
                .lock()
                .unwrap()
                .append(&self.task, &TaskEvent::Paused)
                .unwrap();
        }
        self.inner.run(req, ctx).await
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.inner.current_workspace(task)
    }
}

#[tokio::test]
async fn a_resumed_run_replays_the_model_calls_without_sending_again() {
    let (env, exec, provider) = flow("parser-fix", 12, 10);
    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
    assert_eq!(provider.calls(), 6);
    // A terminal task is returned untouched, even for a fresh agent.
    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);
    assert_eq!(provider.calls(), 6);

    // Paused while the first read is in flight, then resumed with a fresh agent: a new
    // session starts with `Start` and runs to the end.
    let (env, exec, provider) = flow("parser-fix", 12, 10);
    let pausing = PauseOnFirstRead {
        inner: exec,
        writer: Mutex::new(env.second_db()),
        task: env.task.clone(),
        done: AtomicBool::new(false),
    };
    assert_eq!(run_model(&env, &pausing).await, TaskState::Paused);
    assert_eq!(provider.calls(), 2);
    assert_eq!(env.count("AgentTurn"), 4);
    assert_eq!(env.effects("ReadFile")[0].state, EffectState::Completed);

    env.db.append(&env.task, &TaskEvent::Resumed).unwrap();
    assert_eq!(run_model(&env, &pausing).await, TaskState::Succeeded);
    assert_eq!(
        provider.calls(),
        8,
        "two calls before the pause, a whole new session after it"
    );
    assert_eq!(env.count("EffectForfeited"), 0);
    assert!(
        env.effects("ModelCall")
            .iter()
            .all(|r| r.lease_generation == 1)
    );
    let task = env.db.task(&env.task).unwrap();
    assert_eq!(task.verified_digest, Some(task.workspace_digest));
}

#[tokio::test]
async fn a_4xx_is_a_settled_failure_the_agent_retries_until_the_budget_ends() {
    let rejected = ProviderResult::Rejected {
        status: 400,
        body: "bad".into(),
    };
    let (env, exec, provider) = flow_with(FakeProvider::scripted(vec![rejected; 3]), 2, 10);

    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);

    assert_eq!(failed_reason(&env), "budget exhausted");
    let calls = env.effects("ModelCall");
    assert_eq!(calls.len(), 2);
    for rec in &calls {
        assert_eq!(rec.state, EffectState::Failed);
        assert_eq!(rec.lease_generation, 1);
        assert_eq!(
            env.blob_json(&rec.result_digest.unwrap())["reason"],
            "http 400: bad"
        );
    }
    assert_eq!(
        calls[0].request_digest, calls[1].request_digest,
        "the same request is asked again"
    );
    assert_eq!(
        env.db
            .usage_summary(&env.task)
            .unwrap()
            .settled_model_requests,
        2
    );
    let failed = json!({"ModelCallFailed": {"reason": "http 400: bad"}});
    assert!(
        agent_turn_observations(&env).contains(&failed),
        "{:?}",
        agent_turn_observations(&env)
    );
    assert_eq!(provider.calls(), 2);
}

#[tokio::test]
async fn transport_failure_forfeits_and_counts_as_uncertain() {
    let mut script = vec![ProviderResult::Transport("timed out".into())];
    script.extend(fixture_responses("parser-fix-direct"));
    let (env, exec, provider) = flow_with(FakeProvider::scripted(script), 12, 10);

    assert_eq!(run_model(&env, &exec).await, TaskState::Succeeded);

    let calls = env.effects("ModelCall");
    assert_eq!(calls.len(), 5);
    assert_eq!(
        (
            calls[0].state,
            calls[0].result_digest,
            calls[0].lease_generation
        ),
        (EffectState::Failed, None, 1)
    );
    assert!(
        calls[1..]
            .iter()
            .all(|r| r.state == EffectState::Completed && r.lease_generation == 1)
    );
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(
        (
            usage.uncertain_model_requests,
            usage.settled_model_requests,
            usage.reserved_model_requests
        ),
        (1, 4, 0)
    );
    assert_eq!(env.count("EffectForfeited"), 1);
    assert_eq!(agent_turn_observations(&env)[1], json!("ModelCallLost"));
    assert_eq!(
        calls[0].request_digest, calls[1].request_digest,
        "the same body is asked again"
    );
    assert_ne!(calls[0].effect_id, calls[1].effect_id, "under a new effect");
    assert_eq!(model_turns(&env)[..2], [1, 2]);
    assert_eq!(provider.calls(), 5);
}

#[tokio::test]
async fn a_refusal_finishes_and_the_task_fails_without_verified_success() {
    let refusal = ok(json!({
        "id": "msg_r", "type": "message", "role": "assistant", "model": "fake", "stop_reason": "refusal",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "I cannot help with that."}],
    }));
    let (env, exec, provider) = flow_with(FakeProvider::scripted(vec![refusal]), 12, 10);

    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);

    assert_eq!(
        failed_reason(&env),
        "agent finished without verified success"
    );
    assert_eq!(
        env.db
            .usage_summary(&env.task)
            .unwrap()
            .settled_model_requests,
        1
    );
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn without_the_model_capability_the_first_call_fails_the_task() {
    let env = Env::with_caps(10, ALL_CAPS);
    let provider = FakeProvider::scripted(vec![]);
    let counts = agentos_engine::supervised::ExecCounts::default();
    let exec = common::routing_over(
        env.dir.path(),
        env.fixture_exec(),
        Some(Box::new(provider.clone_handle())),
        &counts,
        None,
    );

    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);

    assert_eq!(failed_reason(&env), "capability model.request not granted");
    let denied: Vec<Value> = env
        .events()
        .into_iter()
        .filter(|e| e.event_type == "Denied")
        .map(|e| e.payload)
        .collect();
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["action"], "CallModel");
    assert_eq!(denied[0]["capability"], "model.request");
    assert_eq!(provider.calls(), 0);
    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(
        (
            usage.reserved_model_requests,
            usage.settled_model_requests,
            usage.uncertain_model_requests
        ),
        (0, 0, 0)
    );
    assert!(env.effects("ModelCall").is_empty());
}

fn blob_bytes(env: &Env) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for shard in std::fs::read_dir(env.dir.path().join("blobs/objects")).unwrap() {
        let shard = shard.unwrap();
        for obj in std::fs::read_dir(shard.path()).unwrap() {
            let obj = obj.unwrap();
            if seen.insert(obj.path()) {
                out.push(std::fs::read(obj.path()).unwrap());
            }
        }
    }
    out
}

#[tokio::test]
async fn hostile_tool_inputs_are_data_and_never_touch_host_paths() {
    let hostile = [
        "../../etc/passwd",
        "/etc/passwd",
        "src/__pycache__/x",
        "src/\u{0}x",
    ];
    let mut script = vec![tool_use(0, "list_files", json!({}))];
    for (i, path) in hostile.iter().enumerate() {
        script.push(tool_use(i as u32 + 1, "read_file", json!({ "path": path })));
    }
    script.push(tool_use(5, "read_file", json!({ "path": "src/parser.py" })));
    let patch = common::edit_patch("tests/test_parser.py", "x", "y");
    script.push(tool_use(6, "apply_patch", json!({ "patch": patch })));
    script.push(tool_use(7, "finish", json!({ "summary": "done" })));
    let (env, exec, provider) = flow_with(FakeProvider::scripted(script), 12, 10);

    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);

    assert_eq!(
        failed_reason(&env),
        "agent finished without verified success"
    );
    let invalid = env.denials("InvalidPath");
    assert_eq!(invalid.len(), 4, "{invalid:?}");
    for (denial, path) in invalid.iter().zip(hostile) {
        assert_eq!(denial["action"], "ReadFile");
        assert_eq!(denial["path"], agentos_engine::guestlink::guest_text(path));
    }
    assert_eq!(
        env.effects("ReadFile").len(),
        1,
        "only the legitimate read became an effect"
    );
    assert_eq!(env.denials("PathNotEditable").len(), 1);
    assert_eq!(
        env.db.task(&env.task).unwrap().actions_used,
        3,
        "snapshot, list, one read"
    );
    assert_eq!(provider.calls(), 8);

    // The hostile paths are never joined to a host path: nothing outside the workspace was
    // read (no /etc/passwd content anywhere) and no stray tree appeared in the shadow.
    let leaked = |bytes: &[u8]| bytes.windows(11).any(|w| w == b"root:x:0:0:");
    assert!(!blob_bytes(&env).iter().any(|b| leaked(b)));
    assert!(
        !env.events()
            .iter()
            .any(|e| leaked(e.payload.to_string().as_bytes()))
    );
    let shadow = env
        .dir
        .path()
        .join("shadow")
        .join(env.task.to_string())
        .join("shadow");
    assert!(!shadow.join("etc").exists());
    assert!(!env.dir.path().join("etc").exists());
}

#[tokio::test]
async fn budget_exhaustion_on_a_read_fails_the_task() {
    let (env, exec, _provider) = flow("parser-fix", 12, 2);

    assert_eq!(run_model(&env, &exec).await, TaskState::Failed);

    assert_eq!(failed_reason(&env), "budget exhausted");
    assert_eq!(env.effects("ListFiles").len(), 1);
    assert!(env.effects("ReadFile").is_empty());
}

#[tokio::test]
async fn run_task_is_unchanged_for_the_fake_agent() {
    // The offline path: no provider at all, the deterministic agent, the same routing.
    let env = Env::new(10);
    let counts = agentos_engine::supervised::ExecCounts::default();
    let exec = common::routing_over(env.dir.path(), env.fixture_exec(), None, &counts, None);
    let mut agent = agentos_engine::agent::FakeAgent::from_fixture_patch(common::fix_patch());
    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded);
}
