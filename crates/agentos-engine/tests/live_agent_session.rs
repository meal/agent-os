//! The live agent session: the real Claude Code CLI, inside the agent guest image on the
//! Firecracker microVM, works on the parser fixture through the broker, and its model requests
//! go to the real Messages API. The host adds the key; the guest holds only a placeholder.
//!
//! Gated like `live_model.rs`: without `AGENTOS_LIVE_MODEL_TESTS=1` the test prints `SKIPPED:`
//! and returns (no network call, no KVM). With it, the run needs the key in the file named by
//! `AGENTOS_API_KEY_FILE` (never argv or an environment value), `AGENTOS_KVM_TESTS=1` with
//! `/dev/kvm`, and `AGENTOS_GUEST_IMAGE` pointing at `agent-cli-py314-v1`; a missing piece panics.
//! Run it with `scripts/acceptance.sh live-agent KEY_FILE`, which is billed once.
//!
//! Cost, in principle: every model request carries the CLI's system prompt and tool list (most
//! of its input tokens) and the conversation so far, and at most [`MAX_OUTPUT_TOKENS`] output
//! tokens. [`MODEL_REQUESTS`] caps the sends: the request after the cap ends the task ("budget
//! exhausted") before anything is sent. A permanent rejection (a 400) ends the task at once.
//!
//! The harness itself is exercised offline under the KVM tier, against the local fake of the
//! Messages API with a fake key: `the_live_agent_harness_runs_offline_against_the_local_fake_api`
//! and `the_live_agent_harness_reports_a_rejection_and_fails`.
//!
//! Every run writes `<AGENTOS_LIVE_RECORDING_DIR>/live-agent-<unix ms>.{json,bundle,summary.json}`:
//! the recording of every model attempt (a rejected call keeps its status and a bounded body),
//! the exported journal and request/response bodies, and a summary. The summary is written
//! before any assertion can fail, so a failed run still says why.

mod common;

use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{EffectId, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::messages::STRIPPED_FIELDS;
use agentos_core::state::TaskState;
use agentos_engine::agent::SessionAgent;
use agentos_engine::crash::RunOptions;
use agentos_engine::executor::Executor;
use agentos_engine::export::export_bundle;
use agentos_engine::firecracker::FirecrackerConfig;
use agentos_engine::job::WorkerConfig;
use agentos_engine::model::anthropic::{ANTHROPIC_BASE_URL, AnthropicProvider};
use agentos_engine::model::fake::{self, Recording};
use agentos_engine::model::provider::{ApiKey, ModelProvider, ProviderResult};
use agentos_engine::runner::run_task_with;
use agentos_engine::supervised::ExecCounts;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use common::http::{Reply, serve};
use common::{HomeGuard, copy_dir, fixtures, kvm, live, routing_over, supervised};
use serde_json::{Value, json};

/// The model the CLI's requests are sent to: the cheapest of the current family. The host
/// rewrites every forwarded body's `model` to this, whatever the CLI asked for.
const LIVE_SESSION_MODEL: &str = "claude-haiku-5-5";
/// Sends allowed to the session, retries included (`model_requests` of the contract).
const MODEL_REQUESTS: u32 = 12;
/// Output tokens per request (`max_output_tokens_per_request`); the forwarded `max_tokens` is
/// clamped to it.
const MAX_OUTPUT_TOKENS: u32 = 4096;
/// The task's deadline, 15 minutes.
const DEADLINE_SECONDS: u32 = 900;
/// The guest's memory: the CLI is a ~250 MB binary (see `kvm_agent.rs`).
const MEMORY_MIB: u32 = 1024;
const IMAGE_ID: &str = "agent-cli-py314-v1";
const CLI: &str = "/opt/agent-cli/claude";
/// The goal of the parser fixture, as `live_model.rs` gives it.
const GOAL: &str = "fix the parser";
const CAPS: &[&str] = &[
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "model.request",
    "agent.session",
];
/// `fixtures/profiles/parser-checks-v1` (pinned by the core golden test, as in `live_model.rs`).
const PROFILE_DIGEST: &str = "9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c";
/// The fixture's real fix, as the CLI's own `sed -i` (the same command `kvm_agent.rs` uses).
const FIX_COMMAND: &str = r#"sed -i -e 's/if not line.strip() or line.startswith/line = line.strip()\n        if not line or line.startswith/' -e 's/result\[key\] = value/result[key.strip()] = value.strip()/' src/parser.py"#;
/// Bytes of a rejected body kept in the summary (the provider already bounds it).
const BODY_EXCERPT: usize = 1024;
const SKIP: &str = "SKIPPED: set AGENTOS_LIVE_MODEL_TESTS=1 and AGENTOS_API_KEY_FILE (with AGENTOS_KVM_TESTS=1 and AGENTOS_GUEST_IMAGE) to run the live agent session: one billed run of the real CLI";
/// The microVM tests run one at a time: each boots a 1 GiB guest.
static ONE_VM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The live gate. `None` (after printing [`SKIP`]) without `AGENTOS_LIVE_MODEL_TESTS`; with it,
/// the key from `AGENTOS_API_KEY_FILE`, which must be usable, and the optional base-url override.
struct LiveGate {
    key: ApiKey,
    endpoint: Option<String>,
}

fn live_gate() -> Option<LiveGate> {
    if !live::enabled(std::env::var("AGENTOS_LIVE_MODEL_TESTS").ok().as_deref()) {
        println!("{SKIP}");
        return None;
    }
    // The file is named in the panic text, its content never.
    let path = std::env::var("AGENTOS_API_KEY_FILE")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "AGENTOS_LIVE_MODEL_TESTS is set but AGENTOS_API_KEY_FILE is not: the key is read \
                 from a file only (scripts/acceptance.sh live-agent KEY_FILE)"
            )
        });
    let raw = live::read_key_file(Path::new(&path))
        .unwrap_or_else(|e| panic!("AGENTOS_API_KEY_FILE={path}: {e}"));
    let key = ApiKey::new(&raw)
        .unwrap_or_else(|e| panic!("AGENTOS_API_KEY_FILE holds an unusable key: {e}"));
    Some(LiveGate {
        key,
        endpoint: std::env::var("AGENTOS_ANTHROPIC_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty()),
    })
}

/// The microVM tier for a live run: required once the live gate is open.
fn live_kvm() -> kvm::Kvm {
    assert!(
        std::env::var_os("AGENTOS_KVM_TESTS").is_some(),
        "AGENTOS_LIVE_MODEL_TESTS is set but AGENTOS_KVM_TESTS is not: the agent session runs on the microVM"
    );
    kvm::require().expect("the live agent session needs the microVM tier")
}

/// The contract of the session: the fixture, the agent image, and the capped limits.
fn contract_json() -> String {
    let caps = serde_json::to_string(CAPS).unwrap();
    format!(
        r#"{{
        "goal": "{GOAL}",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "{IMAGE_ID}",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": {MODEL_REQUESTS},
            "max_output_tokens_per_request": {MAX_OUTPUT_TOKENS},
            "tool_actions": 10,
            "deadline_seconds": {DEADLINE_SECONDS},
            "worker_vcpus": 1,
            "worker_memory_mib": {MEMORY_MIB}
        }}
    }}"#
    )
}

/// One session's home: a fresh root on the guest image's filesystem with the fixture, the
/// jailed worker config of the agent image, and the contract.
struct Home {
    /// First: the VMs are killed and the jails collected before `dir` goes.
    _guard: HomeGuard,
    dir: tempfile::TempDir,
    cfg: FirecrackerConfig,
    contract: Contract,
}

impl Home {
    fn new(kvm: &kvm::Kvm) -> Home {
        let dir = kvm.root();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let mut cfg = kvm.jailed_config(dir.path());
        cfg.memory_mib = MEMORY_MIB;
        let contract = Contract::parse(&contract_json()).unwrap();
        Home {
            _guard: HomeGuard::new(dir.path(), &kvm.cgroup_root),
            dir,
            cfg,
            contract,
        }
    }
}

/// `claude -p <goal>` as `kvm_agent.rs` runs it: headless edits, no network tools.
fn cli_argv() -> Vec<String> {
    [
        CLI,
        "-p",
        GOAL,
        "--permission-mode",
        "bypassPermissions",
        "--disallowedTools",
        "WebFetch",
        "WebSearch",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The provider the session uses: the real one (base url overridden when given), recording
/// every attempt to `recording`.
fn recording_provider(
    key: ApiKey,
    endpoint: Option<&str>,
    recording: PathBuf,
) -> Box<dyn ModelProvider> {
    let mut inner = AnthropicProvider::new(key);
    if let Some(url) = endpoint {
        inner = inner.with_base_url(url);
    }
    Box::new(Recording::new(inner, recording))
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// `<AGENTOS_LIVE_RECORDING_DIR or target/live-transcripts>/live-agent-<unix ms>`: the base of
/// the run's artifacts (`.json`, `.bundle`, `.summary.json`).
fn live_base() -> PathBuf {
    let dir = std::env::var_os("AGENTOS_LIVE_RECORDING_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/live-transcripts")
        });
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("live-agent-{}", unix_millis()))
}

fn excerpt(text: &str) -> String {
    text.chars().take(BODY_EXCERPT).collect()
}

/// One row per model attempt, from the recording: a response's usage, or a rejection's status
/// and bounded body. Also the sums of the usage of the responses.
fn attempts(recording: &Path) -> (Value, u64, u64) {
    if !recording.exists() {
        return (json!([]), 0, 0);
    }
    let rec = match fake::load_recording(recording) {
        Ok(rec) => rec,
        Err(e) => return (json!({ "unreadable": excerpt(&e.to_string()) }), 0, 0),
    };
    let (mut input, mut output) = (0u64, 0u64);
    let rows = rec
        .attempts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let mut row =
                json!({ "attempt": i + 1, "request_digest": a.request_digest.to_string() });
            match &a.outcome {
                ProviderResult::Response(bytes, usage) => {
                    input += usage.input_tokens;
                    output += usage.output_tokens;
                    row["outcome"] = json!("response");
                    row["bytes"] = json!(bytes.len());
                    row["input_tokens"] = json!(usage.input_tokens);
                    row["output_tokens"] = json!(usage.output_tokens);
                }
                ProviderResult::Rejected { status, body }
                | ProviderResult::RejectedWithRetryAfter { status, body, .. } => {
                    row["outcome"] = json!("rejected");
                    row["status"] = json!(status);
                    row["body"] = json!(excerpt(body));
                }
                ProviderResult::Transport(reason) => {
                    row["outcome"] = json!("transport");
                    row["reason"] = json!(excerpt(reason));
                }
            }
            row
        })
        .collect();
    (Value::Array(rows), input, output)
}

/// The checks that a protected verification accepted the workspace on disk, as
/// `live_model.rs` makes them. Each problem is a sentence.
fn verified<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    exec: &E,
    task: &TaskId,
    events: &[StoredEvent],
) -> Vec<String> {
    let mut problems = Vec::new();
    let t = db.task(task).unwrap();
    let ws = match exec.current_workspace(task) {
        Some(Ok(ws)) => Some(ws),
        other => {
            problems.push(format!("the workspace is not readable: {other:?}"));
            None
        }
    };
    if ws.is_some() && t.verified_digest != ws {
        problems.push("the verified digest is not the workspace on disk".into());
    }
    let accepting = events
        .windows(2)
        .find(|w| w[1].event_type == "VerifyPassed" && w[0].event_type == "EffectCompleted");
    let Some(w) = accepting else {
        problems.push("no VerifyPassed follows a completed effect".into());
        return problems;
    };
    let id: EffectId = match serde_json::from_value(w[0].payload["effect_id"].clone()) {
        Ok(id) => id,
        Err(e) => {
            problems.push(format!("the accepting effect id: {e}"));
            return problems;
        }
    };
    let rec = match db.effect(&id) {
        Ok(rec) => rec,
        Err(e) => {
            problems.push(format!("the accepting effect {id}: {e}"));
            return problems;
        }
    };
    if rec.kind.tag() != "run_verification" {
        problems.push(format!("the accepting effect is {}", rec.kind.tag()));
    }
    let evidence: Option<Value> = rec
        .result_digest
        .and_then(|d| blobs.get(&d).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());
    match evidence {
        Some(ev) => {
            if ev["passed"] != true {
                problems.push(format!("the verification did not pass: {ev}"));
            }
            if ev["profile_digest"] != PROFILE_DIGEST {
                problems.push(format!("the verification ran {}", ev["profile_digest"]));
            }
            if ws.is_some_and(|ws| ev["workspace_digest"] != ws.to_string()) {
                problems.push("the verified evidence names another workspace".into());
            }
        }
        None => problems.push("the verification has no readable evidence".into()),
    }
    problems
}

/// The model calls: each is settled once, and the forwarded bodies carry the capped contract
/// (no beta-only fields, `stream` false, `max_tokens` within the cap, the session's model).
fn model_checks(
    db: &Db,
    task: &TaskId,
    events: &[StoredEvent],
    manifest: Option<&agentos_engine::export::Manifest>,
    bundle: &Path,
) -> Vec<String> {
    let mut problems = Vec::new();
    let usage = db.usage_summary(task).unwrap();
    if usage.uncertain_model_requests != 0 {
        problems.push(format!("uncertain model requests: {usage:?}"));
    }
    let mut calls = 0u64;
    for e in events.iter().filter(|e| e.event_type == "EffectIntended") {
        if e.payload["kind"].get("ModelCall").is_none() {
            continue;
        }
        calls += 1;
        match serde_json::from_value::<EffectId>(e.payload["effect_id"].clone()) {
            Ok(id) => match db.effect(&id) {
                Ok(rec) if matches!(rec.state, EffectState::Completed | EffectState::Failed) => {}
                Ok(rec) => {
                    problems.push(format!("model call {id} is {:?}, not settled", rec.state))
                }
                Err(e) => problems.push(format!("model call {id}: {e}")),
            },
            Err(e) => problems.push(format!("a model call's effect id: {e}")),
        }
    }
    for e in events.iter().filter(|e| e.event_type == "EffectDispatched") {
        let Ok(id) = serde_json::from_value::<EffectId>(e.payload["effect_id"].clone()) else {
            continue;
        };
        if db.effect(&id).is_ok_and(|r| r.kind.tag() == "model_call")
            && e.payload["lease_generation"] != 1
        {
            problems.push(format!("a model call was dispatched again: {e:?}"));
        }
    }
    if usage.settled_model_requests != calls {
        problems.push(format!(
            "{calls} model calls journaled but {} settled",
            usage.settled_model_requests
        ));
    }
    if calls > u64::from(MODEL_REQUESTS) {
        problems.push(format!(
            "{calls} model calls past the cap of {MODEL_REQUESTS}"
        ));
    }
    let Some(manifest) = manifest else {
        return problems;
    };
    if manifest.model_calls.len() as u64 != calls {
        problems.push(format!(
            "the bundle holds {} model calls, the journal {calls}",
            manifest.model_calls.len()
        ));
    }
    for call in &manifest.model_calls {
        let path = bundle.join(&call.request_file);
        let body: Value = match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        {
            Ok(body) => body,
            Err(e) => {
                problems.push(format!("{}: {e}", call.request_file));
                continue;
            }
        };
        for field in STRIPPED_FIELDS {
            if body.get(field).is_some() {
                problems.push(format!("{} carries {field}", call.request_file));
            }
        }
        if body["stream"] != false {
            problems.push(format!("{} is not stream:false", call.request_file));
        }
        match body["max_tokens"].as_u64() {
            Some(n) if n <= u64::from(MAX_OUTPUT_TOKENS) => {}
            other => problems.push(format!("{} max_tokens is {other:?}", call.request_file)),
        }
        if body["model"] != LIVE_SESSION_MODEL {
            problems.push(format!("{} model is {}", call.request_file, body["model"]));
        }
    }
    problems
}

/// What one session left behind: the task's end state, the reason when it failed, and the
/// problems the checks found (empty when the run met every check).
struct Session {
    state: TaskState,
    problems: Vec<String>,
    summary: PathBuf,
}

/// Runs the real CLI for the parser fixture to its end and checks what it left. The summary
/// (`<base>.summary.json`), the recording (`<base>.json`, written by `provider`) and the bundle
/// (`<base>.bundle/`) are written whatever the outcome. The key must appear in none of them,
/// nor in the blobs or the database: that check panics.
async fn run_session(
    provider: Box<dyn ModelProvider>,
    kvm: &kvm::Kvm,
    key: &str,
    endpoint: &str,
    base: &Path,
) -> Session {
    let home = Home::new(kvm);
    let root = home.dir.path();
    let db = Db::open(&root.join("agentos.db")).unwrap();
    let blobs = BlobStore::open(root.join("blobs")).unwrap();
    let task = db
        .create_task(
            &home.contract,
            &Digest::of(&serde_json::to_vec(&home.contract).unwrap()),
        )
        .unwrap();
    db.append_audit(
        &task,
        "Submitted",
        &json!({
            "repository_digest": workspace_digest(&root.join("snapshot")).unwrap(),
            "profile_digest": workspace_digest(&root.join("profile")).unwrap(),
            "model": format!("anthropic:{LIVE_SESSION_MODEL}"),
            "model_policy_version": 1, "model_limits_version": 1, "model_endpoint": endpoint,
            "worker": "firecracker", "guest_image_digest": home.cfg.image_digest, "jailed": true
        }),
    )
    .unwrap();
    db.approve_task(&task).unwrap();

    let counts = ExecCounts::default();
    let jobs = supervised(
        &root.join("jobs"),
        WorkerConfig::Firecracker(home.cfg.clone()),
        &counts,
        None,
        &[],
    );
    let exec = routing_over(root, jobs, Some(provider), &counts, None);
    let mut agent = SessionAgent::new(cli_argv(), Vec::new(), LIVE_SESSION_MODEL);
    let run = run_task_with(
        &db,
        &blobs,
        &exec,
        &mut agent,
        &task,
        &RunOptions::default(),
    )
    .await;
    let mut problems = Vec::new();
    if let Err(e) = &run {
        problems.push(format!("the engine stopped: {e}"));
    }
    let state = db.task(&task).unwrap().state;
    let events = db.events(&task).unwrap();
    let mut reason = None;
    if let Some(e) = events.iter().rev().find(|e| e.event_type == "Failed") {
        reason = Some(excerpt(&e.payload.to_string()));
    }
    if state == TaskState::Succeeded {
        problems.extend(verified(&db, &blobs, &exec, &task, &events));
    } else {
        problems.push(format!(
            "the task ended {state:?}: {}",
            reason.as_deref().unwrap_or("no reason journaled")
        ));
    }

    let bundle = root.join("bundle");
    let manifest = match export_bundle(&db, &blobs, &task, &bundle) {
        Ok(m) => Some(m),
        Err(e) => {
            problems.push(format!("the journal did not export: {e}"));
            None
        }
    };
    problems.extend(model_checks(
        &db,
        &task,
        &events,
        manifest.as_ref(),
        &bundle,
    ));

    let recording = base.with_extension("json");
    let (rows, input, output) = attempts(&recording);
    let mut event_types = std::collections::BTreeMap::new();
    for e in &events {
        *event_types.entry(e.event_type.clone()).or_insert(0usize) += 1;
    }
    let summary = base.with_extension("summary.json");
    let doc = json!({
        "schema_version": 1, "kind": "live-agent-session",
        "commit": std::env::var("AGENTOS_EVIDENCE_COMMIT").ok(),
        "model": LIVE_SESSION_MODEL, "endpoint": endpoint, "image": IMAGE_ID,
        "contract": {
            "model_requests": MODEL_REQUESTS, "max_output_tokens_per_request": MAX_OUTPUT_TOKENS,
            "deadline_seconds": DEADLINE_SECONDS, "worker_memory_mib": MEMORY_MIB
        },
        "state": format!("{state:?}"), "failure": reason,
        "usage": { "input_tokens": input, "output_tokens": output },
        "events": event_types,
        "model_calls": manifest.as_ref().map_or(Value::Null, |m| json!(m.model_calls)),
        "attempts": rows,
        "problems": problems,
    });
    std::fs::write(&summary, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    if manifest.is_some() {
        copy_dir(&bundle, &base.with_extension("bundle"));
    }

    // Before the caller sees the result: no artifact and no store holds the key.
    let artifacts: Vec<PathBuf> = [
        recording.clone(),
        base.with_extension("bundle"),
        summary.clone(),
        root.join("blobs"),
        root.join("agentos.db"),
        root.join("agentos.db-wal"),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .collect();
    let refs: Vec<&Path> = artifacts.iter().map(PathBuf::as_path).collect();
    live::assert_key_absent(key, &refs);
    Session {
        state,
        problems,
        summary,
    }
}

/// The live run: the real CLI through the broker to the real Messages API.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_cli_fixes_the_parser_in_the_microvm_through_the_real_api() {
    let Some(gate) = live_gate() else { return };
    let kvm = live_kvm();
    let _one = ONE_VM.lock().await;
    let base = live_base();
    let endpoint = gate
        .endpoint
        .clone()
        .unwrap_or_else(|| ANTHROPIC_BASE_URL.to_string());
    let key = gate.key.expose().to_string();
    let provider = recording_provider(
        gate.key,
        gate.endpoint.as_deref(),
        base.with_extension("json"),
    );
    let session = run_session(provider, &kvm, &key, &endpoint, &base).await;
    println!("model: {LIVE_SESSION_MODEL} (cap {MODEL_REQUESTS} requests)");
    println!("task: {:?}", session.state);
    println!("summary: {}", session.summary.display());
    assert!(
        session.problems.is_empty(),
        "the live agent run failed (summary: {}):\n  - {}",
        session.summary.display(),
        session.problems.join("\n  - ")
    );
}

const FAKE_KEY: &str = "sk-ant-FAKE-offline-agent-harness-key";

/// A canned Messages answer with a 200.
fn canned(content: Value, stop_reason: &str) -> (u16, Vec<u8>) {
    let body = json!({
        "id": "msg_offline", "type": "message", "role": "assistant",
        "model": LIVE_SESSION_MODEL, "content": content, "stop_reason": stop_reason,
        "stop_sequence": null, "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    (200, serde_json::to_vec(&body).unwrap())
}

/// The harness against the local fake of the Messages API, with a fake key: turn 1 is the
/// fixture's fix as a `Bash` tool call, turn 2 `end_turn`. The same checks as the live run.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_agent_harness_runs_offline_against_the_local_fake_api() {
    let Some(kvm) = kvm::require() else { return };
    let _one = ONE_VM.lock().await;
    let api = serve(Reply::Sequence(vec![
        canned(
            json!([{"type": "tool_use", "id": "toolu_fix", "name": "Bash",
                    "input": {"command": FIX_COMMAND, "description": "apply the parser fix"}}]),
            "tool_use",
        ),
        canned(
            json!([{"type": "text", "text": "The parser is fixed."}]),
            "end_turn",
        ),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("offline-agent");
    let provider = recording_provider(
        ApiKey::new(FAKE_KEY).unwrap(),
        Some(&api.url()),
        base.with_extension("json"),
    );
    let session = run_session(provider, &kvm, FAKE_KEY, &api.url(), &base).await;
    assert!(session.problems.is_empty(), "{:#?}", session.problems);
    assert_eq!(session.state, TaskState::Succeeded);
    assert_eq!(api.hits(), 2, "one tool turn and one end turn");
    for r in api.requests() {
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == FAKE_KEY),
            "the host adds the key to every forwarded request"
        );
    }
    let summary: Value = serde_json::from_slice(&std::fs::read(&session.summary).unwrap()).unwrap();
    assert_eq!(summary["state"], "Succeeded");
    assert_eq!(summary["model_calls"].as_array().map(Vec::len), Some(2));
    assert_eq!(summary["attempts"][1]["outcome"], "response");
}

/// The same harness, the fake API answering 400 with an Anthropic error body: the task fails
/// at once (a permanent rejection), the recording and the summary keep the status and message,
/// and the failure is reported as a problem rather than a pass.
#[tokio::test(flavor = "multi_thread")]
async fn the_live_agent_harness_reports_a_rejection_and_fails() {
    let Some(kvm) = kvm::require() else { return };
    let _one = ONE_VM.lock().await;
    let rejection = r#"{"type":"error","error":{"type":"invalid_request_error","message":"offline harness: rejected on purpose"}}"#;
    let api = serve(Reply::Sequence(vec![(400, rejection.as_bytes().to_vec())]));
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("offline-rejected");
    let provider = recording_provider(
        ApiKey::new(FAKE_KEY).unwrap(),
        Some(&api.url()),
        base.with_extension("json"),
    );
    let session = run_session(provider, &kvm, FAKE_KEY, &api.url(), &base).await;
    println!("problems of the rejected run: {:#?}", session.problems);
    assert_eq!(session.state, TaskState::Failed, "{:#?}", session.problems);
    assert!(
        session
            .problems
            .iter()
            .any(|p| p.starts_with("the task ended Failed")),
        "{:#?}",
        session.problems
    );
    assert_eq!(api.hits(), 1, "a permanent rejection is not retried");
    let summary: Value = serde_json::from_slice(&std::fs::read(&session.summary).unwrap()).unwrap();
    let row = &summary["attempts"][0];
    assert_eq!(row["outcome"], "rejected", "{summary}");
    assert_eq!(row["status"], 400, "{summary}");
    assert!(
        row["body"]
            .as_str()
            .is_some_and(|b| b.contains("rejected on purpose")),
        "{summary}"
    );
    // The recording itself keeps the same answer (the replayable artifact).
    let recorded = fake::load_recording(&base.with_extension("json")).unwrap();
    assert!(matches!(
        recorded.attempts[0].outcome,
        ProviderResult::Rejected { status: 400, .. }
    ));
}

/// A recording of a session replays through `FakeProvider` (the recording is the artifact the
/// controller reads). Keeps the summary's `attempts` honest about the recorded outcomes.
#[test]
fn the_summary_rows_match_the_recording_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.json");
    let attempts_json = json!({
        "schema_version": 2,
        "attempts": [
            {"request_digest": Digest::of(b"a").to_string(),
             "outcome": {"Rejected": {"status": 429, "body": "slow"}}},
            {"request_digest": Digest::of(b"b").to_string(),
             "outcome": {"Response": [[123, 125], {"input_tokens": 7, "output_tokens": 3}]}},
            {"request_digest": Digest::of(b"c").to_string(),
             "outcome": {"Transport": "reset"}}
        ]
    });
    std::fs::write(&path, attempts_json.to_string()).unwrap();
    let (rows, input, output) = attempts(&path);
    assert_eq!((input, output), (7, 3));
    assert_eq!(rows[0]["status"], 429);
    assert_eq!(rows[0]["body"], "slow");
    assert_eq!(rows[1]["bytes"], 2);
    assert_eq!(rows[2]["outcome"], "transport");
    let (missing, _, _) = attempts(&dir.path().join("none.json"));
    assert_eq!(missing, json!([]));
}
