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
use common::{Env, contract_model, routing_over};

const GOOD: &[u8] = br#"{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;

#[tokio::test]
async fn definite_failures_retain_typed_classification_with_matching_digest() {
    for (status, class) in [(401, "Permanent"), (429, "Transient"), (529, "Transient")] {
        let dir = tempfile::tempdir().unwrap();
        let (ex, _) = exec(
            dir.path(),
            vec![ProviderResult::Rejected {
                status,
                body: "denied".into(),
            }],
            &ExecCounts::default(),
        );
        let r = req(model_kind());
        let out = ex.run(&r, &ctx()).await;
        let value: serde_json::Value = serde_json::from_slice(&out.output).unwrap();
        assert_eq!(value["failure"]["class"], class);
        assert_eq!(out.receipt.result_digest, Some(Digest::of(&out.output)));
        assert_eq!(ex.retained_outcome(&r.effect_id), Some(out));
    }
}

fn contract() -> Contract {
    contract_model(3, 5).0
}

fn model_kind() -> EffectKind {
    EffectKind::ModelCall {
        model: "fake".into(),
        turn: 1,
    }
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
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        worker: "model".into(),
    }
}

fn exec(
    root: &Path,
    results: Vec<ProviderResult>,
    counts: &ExecCounts,
) -> (
    ModelExecutor,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let provider = FakeProvider::scripted(results);
    let calls = provider.calls_counter();
    (
        ModelExecutor::new(root.to_path_buf(), Some(Box::new(provider)), counts.clone()),
        calls,
    )
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
    let (ex, _) = exec(
        dir.path(),
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &counts,
    );
    let (r, c) = (req(model_kind()), ctx());

    let out = ex.run(&r, &c).await;

    assert_eq!(out.receipt.outcome, Outcome::Success);
    assert_eq!(out.output, GOOD);
    assert_eq!(out.receipt.lease_generation, 1);
    assert!(!out.unresolved);
    let file = ex
        .retention_dir(&r.effect_id, &c.attempt_id)
        .join("response.json");
    assert_eq!(
        dir.path()
            .join(format!("{}-{}", r.effect_id, c.attempt_id))
            .join("response.json"),
        file
    );
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
    let rejected = ProviderResult::Rejected {
        status: 400,
        body: "bad\nrequest".into(),
    };
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
    let (ex, _) = exec(
        dir.path(),
        vec![ProviderResult::Transport("timed out".into())],
        &counts,
    );
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
    for body in [
        &b"not json"[..],
        &b"{\"stop_reason\":1}"[..],
        &b"{\"content\":[]}"[..],
        &b"[]"[..],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (ex, _) = exec(
            dir.path(),
            vec![ProviderResult::Response(body.to_vec(), Usage::default())],
            &ExecCounts::default(),
        );
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
    let (ex, calls) = exec(
        dir.path(),
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &counts,
    );
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
    let (ex, calls) = exec(
        dir.path(),
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &counts,
    );
    let ex = ex.with_crash(Some(hook.clone()));
    let r = req(model_kind());

    let out = ex.run(&r, &ctx()).await;

    assert!(
        failure_reason(&out).starts_with("injected crash"),
        "{out:?}"
    );
    assert_eq!(hook.tripped(), Some(CrashPoint::DuringExecute));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(counts.get("model_call"), 1);
    assert_eq!(ex.retained_outcome(&r.effect_id), None);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "nothing retained"
    );
}

#[tokio::test]
async fn retained_outcome_ignores_other_effects_and_unresolved_files() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(
        dir.path(),
        vec![
            ProviderResult::Transport("x".into()),
            ProviderResult::Response(GOOD.to_vec(), Usage::default()),
        ],
        &counts,
    );
    let other = req(model_kind());
    let mut mine = req(EffectKind::ModelCall {
        model: "fake".into(),
        turn: 2,
    });
    mine.effect_id = EffectId::derive(&mine.task_id, 1, &mine.kind, &Digest::of(b"mine"));
    let c = ctx();

    let unresolved = ex.run(&other, &c).await;
    assert!(unresolved.unresolved);
    // An unresolved outcome written by hand is still never returned.
    let d = ex.retention_dir(&other.effect_id, &c.attempt_id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("response.json"),
        serde_json::to_vec(&unresolved).unwrap(),
    )
    .unwrap();
    assert_eq!(ex.retained_outcome(&other.effect_id), None);

    let kept = ex.run(&mine, &ctx()).await;
    assert_eq!(ex.retained_outcome(&mine.effect_id), Some(kept));
    assert_eq!(
        ex.retained_outcome(&other.effect_id),
        None,
        "another effect's file is not mine"
    );
}

#[tokio::test]
async fn the_highest_lease_generation_wins() {
    let dir = tempfile::tempdir().unwrap();
    let counts = ExecCounts::default();
    let (ex, _) = exec(dir.path(), vec![], &counts);
    let r = req(model_kind());
    let mut best = None;
    for generation in [2u64, 1, 3] {
        let c = AttemptCtx {
            attempt_id: AttemptId::new(),
            lease_generation: generation,
            worker: "m".into(),
        };
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
    env.db
        .append(&env.task, &TaskEvent::WorkspaceUpdated { digest: base })
        .unwrap();
    let counts = ExecCounts::default();
    let provider: Box<dyn ModelProvider> =
        Box::new(FakeProvider::scripted(vec![ProviderResult::Response(
            GOOD.to_vec(),
            Usage::default(),
        )]));
    let routing = routing_over(
        env.dir.path(),
        env.fixture_exec(),
        Some(provider),
        &counts,
        None,
    );

    let snap = req_of(&env.task, EffectKind::ReadSnapshot, b"", &env.contract);
    let out = routing.run(&snap, &ctx()).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{out:?}");
    assert_eq!(
        out.new_workspace,
        Some(base),
        "the fixture executor answers a snapshot read"
    );

    let model = req_of(&env.task, model_kind(), b"{\"messages\":[]}", &env.contract);
    let out = routing.run(&model, &ctx()).await;
    assert_eq!(out.output, GOOD);
    assert_eq!(routing.retained_outcome(&model.effect_id), Some(out));
    assert_eq!(counts.get("model_call"), 1);
    assert_eq!(routing.retained_outcome(&snap.effect_id), None);

    let list = req_of(
        &env.task,
        EffectKind::ListFiles { turn: 1 },
        b"",
        &env.contract,
    );
    let out = routing.run(&list, &ctx()).await;
    assert_eq!(out.receipt.outcome, Outcome::Success, "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.output).unwrap();
    let want: Vec<String> = list_files(&env.snapshot_dir())
        .unwrap()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(v["files"], serde_json::json!(want));
    assert_eq!(out.new_workspace, None);
}

#[tokio::test]
async fn a_malformed_id_is_never_joined_into_a_retention_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("model");
    let (ex, _) = exec(
        &root,
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &ExecCounts::default(),
    );
    let mut r = req(model_kind());
    r.effect_id = serde_json::from_str("\"../../escape\"").unwrap();

    let out = ex.run(&r, &ctx()).await;

    assert_eq!(
        out.receipt.outcome,
        Outcome::Success,
        "the outcome is still returned"
    );
    assert!(
        !dir.path().join("escape").exists() && !root.exists(),
        "nothing written for a bad id"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn retention_creates_a_missing_root_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("a").join("model");
    let (ex, _) = exec(
        &root,
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &ExecCounts::default(),
    );
    let r = req(model_kind());
    let out = ex.run(&r, &ctx()).await;
    assert_eq!(ex.retained_outcome(&r.effect_id), Some(out));
}

/// A retained, resolved outcome for `req` written by hand under `attempt`.
fn plant(ex: &ModelExecutor, req: &EffectRequest, attempt: &AttemptId, generation: u64) {
    let mut out = ExecOutcome::success(
        req,
        &AttemptCtx {
            attempt_id: attempt.clone(),
            lease_generation: generation,
            worker: "model".into(),
        },
        GOOD.to_vec(),
    );
    out.receipt.lease_generation = generation;
    let dir = ex.retention_dir(&req.effect_id, attempt);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("response.json"), serde_json::to_vec(&out).unwrap()).unwrap();
}

fn effect_n(n: u32) -> EffectRequest {
    let mut r = req(model_kind());
    r.effect_id = EffectId::derive(&r.task_id, n, &r.kind, &Digest::of(&n.to_le_bytes()));
    r
}

#[test]
fn the_right_attempt_is_found_among_many_unrelated_entries() {
    let dir = tempfile::tempdir().unwrap();
    let ex = ModelExecutor::new(dir.path().to_path_buf(), None, ExecCounts::default());
    // Many unrelated retained effects, each with a corrupt file: reading any of them would log.
    for n in 0..300 {
        let other = effect_n(n);
        let d = ex.retention_dir(&other.effect_id, &AttemptId::new());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("response.json"), b"not json").unwrap();
    }
    let mine = effect_n(1000);
    plant(&ex, &mine, &AttemptId::new(), 1);
    plant(&ex, &mine, &AttemptId::new(), 3);
    plant(&ex, &mine, &AttemptId::new(), 2);

    let logs = capture_warnings(|| {
        let found = ex.retained_outcome(&mine.effect_id).unwrap();
        assert_eq!(found.receipt.lease_generation, 3, "the newest lease wins");
        assert_eq!(ex.retained_outcome(&effect_n(2000).effect_id), None);
    });

    assert_eq!(logs, "", "only this effect's entries are opened");
}

#[tokio::test]
async fn an_entry_planted_right_after_a_run_is_found_by_the_next_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let (ex, _) = exec(
        dir.path(),
        vec![ProviderResult::Response(GOOD.to_vec(), Usage::default())],
        &ExecCounts::default(),
    );
    let late = effect_n(2);
    assert_eq!(ex.retained_outcome(&late.effect_id), None);

    let run = req(model_kind());
    let kept = ex.run(&run, &ctx()).await;
    assert_eq!(ex.retained_outcome(&run.effect_id), Some(kept));

    // No pause: a lookup never depends on how much time has passed since the last one.
    plant(&ex, &late, &AttemptId::new(), 1);
    assert!(ex.retained_outcome(&late.effect_id).is_some());
}

#[test]
fn a_corrupt_response_of_one_effect_does_not_affect_another() {
    let dir = tempfile::tempdir().unwrap();
    let ex = ModelExecutor::new(dir.path().to_path_buf(), None, ExecCounts::default());
    let (bad, good) = (effect_n(1), effect_n(2));
    let d = ex.retention_dir(&bad.effect_id, &AttemptId::new());
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("response.json"), b"{ truncated").unwrap();
    plant(&ex, &good, &AttemptId::new(), 1);

    let logs = capture_warnings(|| {
        assert_eq!(ex.retained_outcome(&bad.effect_id), None);
        assert!(ex.retained_outcome(&good.effect_id).is_some());
    });

    assert!(logs.contains("cannot parse"), "{logs}");
    assert!(logs.contains(bad.effect_id.as_str()), "{logs}");
    assert!(!logs.contains(good.effect_id.as_str()), "{logs}");
}

#[test]
fn an_unreadable_response_is_logged_and_treated_as_nothing_retained() {
    let dir = tempfile::tempdir().unwrap();
    let ex = ModelExecutor::new(dir.path().to_path_buf(), None, ExecCounts::default());
    let (unreadable, missing) = (effect_n(1), effect_n(2));
    // `response.json` is a directory: reading it fails with an error that is not NotFound.
    let d = ex.retention_dir(&unreadable.effect_id, &AttemptId::new());
    std::fs::create_dir_all(d.join("response.json")).unwrap();
    // A retention directory without a response is an ordinary crash leftover: silent.
    std::fs::create_dir_all(ex.retention_dir(&missing.effect_id, &AttemptId::new())).unwrap();

    let logs = capture_warnings(|| {
        assert_eq!(ex.retained_outcome(&unreadable.effect_id), None);
        assert_eq!(ex.retained_outcome(&missing.effect_id), None);
    });

    assert!(logs.contains("cannot read"), "{logs}");
    assert!(logs.contains(unreadable.effect_id.as_str()), "{logs}");
    assert!(!logs.contains(missing.effect_id.as_str()), "{logs}");
}

/// The warnings `f` logs, as text.
fn capture_warnings(f: impl FnOnce()) -> String {
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink(Arc::default());
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = sink.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}
