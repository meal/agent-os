//! The live model run: a real model fixes the parser fixture through the broker on the host
//! worker, and its recording replays offline. Gated like the KVM tier: without
//! `AGENTOS_LIVE_MODEL_TESTS` both tests print `SKIPPED:` and return (no network call);
//! with it set and no key they panic. See `common::live`.
//!
//! The harness itself (world, assertions, recording, replay) is exercised offline by
//! `the_live_harness_runs_offline_against_the_local_fake_api`, with a clearly fake key and
//! the base-url override pointing at the local fake of the Messages API.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::effect::EffectId;
use agentos_core::state::TaskState;
use agentos_engine::agent::ModelAgent;
use agentos_engine::model::anthropic::AnthropicProvider;
use agentos_engine::model::fake::{FakeProvider, Recording};
use agentos_engine::model::provider::{ApiKey, ModelProvider};
use agentos_engine::runner::run_task;
use agentos_engine::supervised::ExecCounts;
use agentos_engine::job::WorkerConfig;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use common::http::{serve, Reply};
use common::live::{self, Live};
use common::{copy_dir, fixtures, host_config, routing_over, supervised, transcript};

/// The digest of `fixtures/profiles/parser-checks-v1` (pinned by the core golden test).
const PROFILE_DIGEST: &str = "9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c";

/// What one run left behind, for the checks that compare two runs.
#[derive(Debug)]
struct Run {
    /// Model calls settled by the run (`usage.settled_model_requests`).
    model_calls: usize,
    /// Journal events by type.
    event_types: BTreeMap<String, usize>,
    duration: Duration,
}

/// A fresh host-worker world over supervised jobs, then `run_task` with a `ModelAgent` for
/// `model` answering through `provider`. Every invariant of the live run is asserted here, so
/// the offline harness test and the replay check exactly what the live run checks.
async fn execute(provider: Box<dyn ModelProvider>, model: &str) -> Run {
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    copy_dir(&fixtures().join("parser-repo"), &root.join("snapshot"));
    copy_dir(&fixtures().join("profiles/parser-checks-v1"), &root.join("profile"));
    let db = Db::open(&root.join("agentos.db")).unwrap();
    let blobs = BlobStore::open(root.join("blobs")).unwrap();
    let (contract, digest) = live::contract();
    let task = db.create_task(&contract, &digest).unwrap();
    db.approve_task(&task).unwrap();

    let counts = ExecCounts::default();
    let jobs = supervised(&root.join("jobs"), WorkerConfig::Host(host_config(root)), &counts, None, &[]);
    let exec = routing_over(root, jobs, Some(provider), &counts, None);
    let mut agent = ModelAgent::new(contract, model);
    let state = run_task(&db, &blobs, &exec, &mut agent, &task).await.unwrap();
    assert_eq!(state, TaskState::Succeeded, "the model did not fix the fixture: {:?}", db.task(&task).unwrap());

    let t = db.task(&task).unwrap();
    let ws = workspace_digest(&root.join("work").join(task.as_str()).join("ws")).unwrap();
    assert_eq!(t.verified_digest, Some(ws), "the verified digest is the workspace on disk");
    assert_eq!(t.workspace_digest, ws);

    let events = db.events(&task).unwrap();
    // The verification whose completion committed VerifyPassed (the same transaction, so its
    // EffectCompleted is the event right before) was a RunVerification effect, never a model response.
    let accepting = events
        .windows(2)
        .find(|w| w[1].event_type == "VerifyPassed" && w[0].event_type == "EffectCompleted")
        .expect("VerifyPassed follows an EffectCompleted");
    let id: EffectId = serde_json::from_value(accepting[0].payload["effect_id"].clone()).unwrap();
    let rec = db.effect(&id).unwrap();
    assert_eq!(rec.kind.tag(), "run_verification", "{rec:?}");
    let evidence: serde_json::Value = serde_json::from_slice(&blobs.get(&rec.result_digest.unwrap()).unwrap()).unwrap();
    assert_eq!(evidence["passed"], true, "{evidence}");
    assert_eq!(evidence["profile_digest"], PROFILE_DIGEST, "{evidence}");
    assert_eq!(evidence["workspace_digest"], ws.to_string(), "{evidence}");

    let usage = db.usage_summary(&task).unwrap();
    assert!((2..=12).contains(&usage.settled_model_requests), "settled model requests: {usage:?}");
    assert_eq!(usage.uncertain_model_requests, 0, "{usage:?}");
    let mut dispatched = 0;
    for e in events.iter().filter(|e| e.event_type == "EffectDispatched") {
        let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
        if db.effect(&id).unwrap().kind.tag() == "model_call" {
            dispatched += 1;
            assert_eq!(e.payload["lease_generation"], 1, "a model call was dispatched again: {e:?}");
        }
    }
    assert_eq!(dispatched as u64, usage.settled_model_requests, "every settled call was dispatched once");

    let mut event_types = BTreeMap::new();
    for e in &events {
        *event_types.entry(e.event_type.clone()).or_insert(0) += 1;
    }
    Run { model_calls: usage.settled_model_requests as usize, event_types, duration: started.elapsed() }
}

fn summary(run: &Run) -> String {
    run.event_types.iter().map(|(t, n)| format!("{t}={n}")).collect::<Vec<_>>().join(" ")
}

fn unix_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// `target/live-transcripts/<unix ms>.json` of the workspace the tests run from.
fn recording_path() -> PathBuf {
    // `Recording` does not create directories (a failed write is only a warning), so make it here.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/live-transcripts");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{}.json", unix_millis()))
}

/// A recording provider over the Anthropic provider for `key`, at `base_url` when given.
fn recording_over(key: ApiKey, base_url: Option<&str>, path: PathBuf) -> Box<dyn ModelProvider> {
    let mut inner = AnthropicProvider::new(key);
    if let Some(url) = base_url {
        inner = inner.with_base_url(url);
    }
    Box::new(Recording::new(inner, path))
}

/// Runs `execute` on a thread of its own (the two gated tests share one live run, whichever
/// starts first; the other waits for it and reuses its recording).
fn run_on_thread(live: Live, path: PathBuf) -> Result<(Run, PathBuf), String> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let provider = recording_over(live.key, live.base_url.as_deref(), path.clone());
        (rt.block_on(execute(provider, &live.model)), path)
    })
    .join()
    .map_err(|p| {
        p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "the live run panicked".into())
    })
}

static LIVE_RUN: OnceLock<Result<(Run, PathBuf), String>> = OnceLock::new();

fn live_run(live: Live) -> &'static (Run, PathBuf) {
    match LIVE_RUN.get_or_init(|| run_on_thread(live, recording_path())) {
        Ok(run) => run,
        Err(why) => panic!("the live run failed: {why}"),
    }
}

#[test]
fn a_real_model_fixes_the_fixture_through_the_broker_on_the_host_worker() {
    let Some(live) = live::require() else { return };
    let model = live.model.clone();
    let (run, path) = live_run(live);
    println!("model: {model}");
    println!("model calls: {} in {:.1}s", run.model_calls, run.duration.as_secs_f64());
    println!("recorded transcript: {}", path.display());
    println!("journal: {}", summary(run));
}

#[tokio::test]
async fn the_live_run_replays_offline_from_its_recording() {
    let Some(live) = live::require() else { return };
    let model = live.model.clone();
    // The shared run blocks until the first of the two tests has finished it.
    let (run, path) = tokio::task::spawn_blocking(move || live_run(live)).await.unwrap();
    let replay = execute(Box::new(FakeProvider::from_file(path).unwrap()), &model).await;
    assert_eq!(replay.model_calls, run.model_calls, "the replay makes as many model calls as the recording holds");
    println!("replayed {} model calls from {}: {}", replay.model_calls, path.display(), summary(&replay));
}

/// The harness against the local fake API with a clearly fake key: no network beyond
/// 127.0.0.1, no real key. The run records its transcript; the recording replays offline.
#[tokio::test]
async fn the_live_harness_runs_offline_against_the_local_fake_api() {
    let api = serve(Reply::Transcript(transcript("parser-fix-direct")));
    let key = ApiKey::new("sk-ant-FAKE-offline-harness-key").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");

    let run = execute(recording_over(key, Some(&api.url()), path.clone()), "claude-opus-5-5").await;
    assert_eq!(run.model_calls, 4, "list, read, patch, verify");
    assert_eq!(api.hits(), 4);
    for r in api.requests() {
        assert!(r.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("x-api-key") && v == "sk-ant-FAKE-offline-harness-key"));
    }

    let recorded = agentos_engine::model::fake::load_transcript(&path).unwrap();
    assert_eq!(recorded.responses.len(), 4);
    assert!(recorded.responses.iter().all(|e| e.expect_request_digest.is_some()), "every entry pins its request");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("FAKE-offline-harness-key"), "the recording never holds the key");

    let replay = execute(Box::new(FakeProvider::from_file(&path).unwrap()), "claude-opus-5-5").await;
    assert_eq!(replay.model_calls, run.model_calls);
    assert_eq!(replay.event_types, run.event_types, "the replay journals the same events");
}
