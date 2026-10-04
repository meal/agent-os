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
use agentos_core::ids::Digest;
use agentos_core::state::TaskState;
use agentos_engine::agent::ModelAgent;
use agentos_engine::executor::Executor;
use agentos_engine::export::export_bundle;
use agentos_engine::job::WorkerConfig;
use agentos_engine::model::anthropic::AnthropicProvider;
use agentos_engine::model::fake::{FakeProvider, Recording};
use agentos_engine::model::provider::{ApiKey, ModelProvider};
use agentos_engine::runner::run_task;
use agentos_engine::supervised::ExecCounts;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use common::http::{Reply, serve};
use common::live::{self, Live};
use common::{copy_dir, fixtures, host_config, routing_over, supervised, transcript};

/// The digest of `fixtures/profiles/parser-checks-v1` (pinned by the core golden test).
const PROFILE_DIGEST: &str = "9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c";

#[derive(Clone, Copy)]
enum WorkerTier {
    Host,
    FakeJailed,
    RealJailed,
}

impl WorkerTier {
    fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::FakeJailed => "fake-jailed",
            Self::RealJailed => "firecracker",
        }
    }
}

/// What one run left behind, for the checks that compare two runs.
#[derive(Debug, serde::Serialize)]
struct Run {
    /// Model calls settled by the run (`usage.settled_model_requests`).
    model_calls: usize,
    /// Journal events by type.
    event_types: BTreeMap<String, usize>,
    duration: Duration,
    requests: Vec<Digest>,
    workspace: Digest,
    patch: Digest,
    profile: Digest,
}

/// A fresh host-worker world over supervised jobs, then `run_task` with a `ModelAgent` for
/// `model` answering through `provider`. Every invariant of the live run is asserted here, so
/// the offline harness test and the replay check exactly what the live run checks.
async fn execute(provider: Box<dyn ModelProvider>, model: &str) -> Run {
    execute_for(
        provider,
        model,
        WorkerTier::Host,
        agentos_engine::model::anthropic::ANTHROPIC_BASE_URL,
    )
    .await
}

async fn execute_for(
    provider: Box<dyn ModelProvider>,
    model: &str,
    tier: WorkerTier,
    endpoint: &str,
) -> Run {
    execute_with_artifacts(provider, model, tier, endpoint, None).await
}

async fn execute_with_artifacts(
    provider: Box<dyn ModelProvider>,
    model: &str,
    tier: WorkerTier,
    endpoint: &str,
    artifacts: Option<&Path>,
) -> Run {
    let started = Instant::now();
    let kvm = if matches!(tier, WorkerTier::RealJailed) {
        Some(common::kvm::require().expect("live firecracker needs AGENTOS_KVM_TESTS=1"))
    } else {
        None
    };
    let dir = match &kvm {
        Some(kvm) => kvm.root(),
        None => tempfile::tempdir().unwrap(),
    };
    let root = dir.path();
    copy_dir(&fixtures().join("parser-repo"), &root.join("snapshot"));
    copy_dir(
        &fixtures().join("profiles/parser-checks-v1"),
        &root.join("profile"),
    );
    let db = Db::open(&root.join("agentos.db")).unwrap();
    let blobs = BlobStore::open(root.join("blobs")).unwrap();
    let (contract, digest) = live::contract();
    let task = db.create_task(&contract, &digest).unwrap();
    let worker = match tier {
        WorkerTier::Host => WorkerConfig::Host(host_config(root)),
        WorkerTier::FakeJailed => {
            WorkerConfig::Firecracker(common::jailed_fake_firecracker_config(root))
        }
        WorkerTier::RealJailed => {
            WorkerConfig::Firecracker(kvm.as_ref().unwrap().jailed_config(root))
        }
    };
    let image = match &worker {
        WorkerConfig::Host(_) | WorkerConfig::Scripted(_) => None,
        WorkerConfig::Firecracker(cfg) => Some(cfg.image_digest),
    };
    db.append_audit(
        &task,
        "Submitted",
        &serde_json::json!({
            "repository_digest": workspace_digest(&root.join("snapshot")).unwrap(),
            "profile_digest": workspace_digest(&root.join("profile")).unwrap(),
            "model": format!("anthropic:{model}"),
            "model_policy_version": 1, "model_limits_version": 1, "model_endpoint": endpoint,
            "worker": if matches!(tier, WorkerTier::Host) { "host" } else { "firecracker" },
            "guest_image_digest": image, "jailed": !matches!(tier, WorkerTier::Host)
        }),
    )
    .unwrap();
    db.approve_task(&task).unwrap();

    let counts = ExecCounts::default();
    let jobs = supervised(
        &root.join("jobs"),
        worker,
        &counts,
        None,
        if matches!(tier, WorkerTier::FakeJailed) {
            &[("AGENTOS_TEST_WORKERS", "1")]
        } else {
            &[]
        },
    );
    let exec = routing_over(root, jobs, Some(provider), &counts, None);
    let mut agent = ModelAgent::new(contract, model);
    let state = run_task(&db, &blobs, &exec, &mut agent, &task)
        .await
        .unwrap();
    // Export failures as well as successes when running requested acceptance.
    let bundle = root.join("bundle");
    let manifest = export_bundle(&db, &blobs, &task, &bundle).unwrap();
    if let Some(destination) = artifacts {
        copy_dir(&bundle, destination);
    }
    assert_eq!(
        state,
        TaskState::Succeeded,
        "the model did not fix the fixture: {:?}",
        db.task(&task).unwrap()
    );

    let t = db.task(&task).unwrap();
    let ws = exec.current_workspace(&task).unwrap().unwrap();
    assert_eq!(
        t.verified_digest,
        Some(ws),
        "the verified digest is the workspace on disk"
    );
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
    let evidence: serde_json::Value =
        serde_json::from_slice(&blobs.get(&rec.result_digest.unwrap()).unwrap()).unwrap();
    assert_eq!(evidence["passed"], true, "{evidence}");
    assert_eq!(evidence["profile_digest"], PROFILE_DIGEST, "{evidence}");
    assert_eq!(evidence["workspace_digest"], ws.to_string(), "{evidence}");

    let usage = db.usage_summary(&task).unwrap();
    assert!(
        (2..=12).contains(&usage.settled_model_requests),
        "settled model requests: {usage:?}"
    );
    assert_eq!(usage.uncertain_model_requests, 0, "{usage:?}");
    let mut dispatched = 0;
    for e in events.iter().filter(|e| e.event_type == "EffectDispatched") {
        let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
        if db.effect(&id).unwrap().kind.tag() == "model_call" {
            dispatched += 1;
            assert_eq!(
                e.payload["lease_generation"], 1,
                "a model call was dispatched again: {e:?}"
            );
        }
    }
    assert_eq!(
        dispatched as u64, usage.settled_model_requests,
        "every settled call was dispatched once"
    );

    let mut event_types = BTreeMap::new();
    for e in &events {
        *event_types.entry(e.event_type.clone()).or_insert(0) += 1;
    }
    Run {
        model_calls: usage.settled_model_requests as usize,
        event_types,
        duration: started.elapsed(),
        requests: manifest
            .model_calls
            .iter()
            .map(|c| c.request_digest)
            .collect(),
        workspace: ws,
        patch: manifest.patch_digest,
        profile: manifest.verification_profile_digest.unwrap(),
    }
}

fn assert_same_replay(run: &Run, replay: &Run) {
    assert_eq!(run.model_calls, replay.model_calls);
    assert_eq!(run.requests, replay.requests);
    assert_eq!(run.workspace, replay.workspace);
    assert_eq!(run.patch, replay.patch);
    assert_eq!(run.profile, replay.profile);
}

fn summary(run: &Run) -> String {
    run.event_types
        .iter()
        .map(|(t, n)| format!("{t}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// `target/live-transcripts/<unix ms>.json` of the workspace the tests run from.
fn recording_path() -> PathBuf {
    // `Recording` does not create directories (a failed write is only a warning), so make it here.
    let dir = std::env::var_os("AGENTOS_LIVE_RECORDING_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/live-transcripts")
        });
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let provider = recording_over(live.key, live.base_url.as_deref(), path.clone());
        let tier = if live.worker == "host" {
            WorkerTier::Host
        } else {
            WorkerTier::RealJailed
        };
        let endpoint = live
            .base_url
            .as_deref()
            .unwrap_or(agentos_engine::model::anthropic::ANTHROPIC_BASE_URL);
        let run = rt.block_on(execute_with_artifacts(
            provider,
            &live.model,
            tier,
            endpoint,
            Some(&path.with_extension("bundle")),
        ));
        let evidence = serde_json::json!({
            "schema_version": 1, "kind": "live-acceptance", "worker": tier.name(),
            "commit": std::env::var("AGENTOS_EVIDENCE_COMMIT").ok(),
            "model": live.model, "endpoint": endpoint, "run": run,
            "uncertain_model_requests": 0, "single_dispatch_verified": true
        });
        std::fs::write(
            path.with_extension("evidence.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
        (run, path)
    })
    .join()
    .map_err(|p| {
        p.downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the live run panicked".into())
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
fn a_real_model_fixes_the_fixture_through_the_broker_on_the_selected_worker() {
    let Some(live) = live::require() else { return };
    let model = live.model.clone();
    let (run, path) = live_run(live);
    println!("model: {model}");
    println!(
        "model calls: {} in {:.1}s",
        run.model_calls,
        run.duration.as_secs_f64()
    );
    println!("recorded transcript: {}", path.display());
    println!("journal: {}", summary(run));
}

#[tokio::test]
async fn the_live_run_replays_offline_from_its_recording() {
    let Some(live) = live::require() else { return };
    let model = live.model.clone();
    let endpoint = live
        .base_url
        .clone()
        .unwrap_or_else(|| agentos_engine::model::anthropic::ANTHROPIC_BASE_URL.into());
    let tier = if live.worker == "host" {
        WorkerTier::Host
    } else {
        WorkerTier::RealJailed
    };
    // The shared run blocks until the first of the two tests has finished it.
    let (run, path) = tokio::task::spawn_blocking(move || live_run(live))
        .await
        .unwrap();
    let replay = execute_for(
        Box::new(FakeProvider::from_file(path).unwrap()),
        &model,
        tier,
        &endpoint,
    )
    .await;
    assert_same_replay(run, &replay);
    std::fs::write(
        path.with_extension("replay.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1, "kind": "offline-replay", "worker": tier.name(), "run": replay,
            "request_patch_workspace_profile_match": true
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        replay.model_calls, run.model_calls,
        "the replay makes as many model calls as the recording holds"
    );
    println!(
        "replayed {} model calls from {}: {}",
        replay.model_calls,
        path.display(),
        summary(&replay)
    );
}

/// The harness against the local fake API with a clearly fake key: no network beyond
/// 127.0.0.1, no real key. The run records its transcript; the recording replays offline.
#[tokio::test]
async fn the_live_harness_runs_offline_against_the_local_fake_api() {
    let api = serve(Reply::Transcript(transcript("parser-fix-direct")));
    let key = ApiKey::new("sk-ant-FAKE-offline-harness-key").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");

    let run = execute(
        recording_over(key, Some(&api.url()), path.clone()),
        "claude-opus-5-5",
    )
    .await;
    assert_eq!(run.model_calls, 4, "list, read, patch, verify");
    assert_eq!(api.hits(), 4);
    for r in api.requests() {
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-api-key")
                    && v == "sk-ant-FAKE-offline-harness-key")
        );
    }

    let recorded = agentos_engine::model::fake::load_transcript(&path).unwrap();
    assert_eq!(recorded.responses.len(), 4);
    assert!(
        recorded
            .responses
            .iter()
            .all(|e| e.expect_request_digest.is_some()),
        "every entry pins its request"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("FAKE-offline-harness-key"),
        "the recording never holds the key"
    );

    let replay = execute(
        Box::new(FakeProvider::from_file(&path).unwrap()),
        "claude-opus-5-5",
    )
    .await;
    assert_same_replay(&run, &replay);
    assert_eq!(
        replay.event_types, run.event_types,
        "the replay journals the same events"
    );
}

#[tokio::test]
async fn the_jailed_live_harness_runs_offline_against_the_local_fake_api() {
    let api = serve(Reply::Transcript(transcript("parser-fix-direct")));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jailed-recording.json");
    let run = execute_for(
        recording_over(
            ApiKey::new("sk-ant-FAKE-jailed-harness-key").unwrap(),
            Some(&api.url()),
            path.clone(),
        ),
        "claude-opus-5-5",
        WorkerTier::FakeJailed,
        &api.url(),
    )
    .await;
    assert_eq!(api.hits(), 4);
    let replay = execute_for(
        Box::new(FakeProvider::from_file(&path).unwrap()),
        "claude-opus-5-5",
        WorkerTier::FakeJailed,
        &api.url(),
    )
    .await;
    assert_eq!(run.model_calls, replay.model_calls);
    assert_eq!(run.requests, replay.requests);
    assert_eq!(run.workspace, replay.workspace);
    assert_eq!(run.patch, replay.patch);
    assert_eq!(run.profile, replay.profile);
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("FAKE-jailed-harness-key")
    );
}

#[test]
fn live_gate_requires_explicit_opt_in_and_bounded_regular_key_file() {
    assert!(!live::enabled(None));
    assert!(!live::enabled(Some("0")));
    assert!(!live::enabled(Some("")));
    assert!(live::enabled(Some("1")));
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    std::fs::write(&key, b"sk-ant-FAKE-gate-key\n").unwrap();
    assert_eq!(live::read_key_file(&key).unwrap(), "sk-ant-FAKE-gate-key\n");
    std::fs::write(&key, vec![b'k'; 4097]).unwrap();
    assert!(live::read_key_file(&key).is_err());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&key, &link).unwrap();
    assert!(live::read_key_file(&link).is_err());
    let fifo = dir.path().join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let started = Instant::now();
    assert!(live::read_key_file(&fifo).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}
