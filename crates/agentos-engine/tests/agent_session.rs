//! A guest agent session through the real runner: the CLI runs as a session job over the fake
//! guest, its model requests are served through the runner as journaled `ModelCall`s (to a
//! fake provider), and its patch goes through `ApplyPatch` and the protected verification.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::EffectRecord;
use agentos_core::effect::EffectState;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::{Agent, AgentAction, Observation, SessionAgent};
use agentos_engine::job::WorkerConfig;
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::ProviderResult;
use agentos_engine::runner::run_task;
use agentos_engine::supervised::ExecCounts;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use common::{
    FnAgent, copy_dir, create_patch, fake_firecracker_config, fixtures, processes_of_home,
    routing_over, supervised,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const CAPS: &[&str] = &[
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "model.request",
    "agent.session",
];

/// The guest's `curl` of the scripted CLI: one model call, body from `$IN`, reply to `$OUT`.
const CURL: &str = "/usr/bin/curl -fsS -o \"$HOME/$OUT\" -X POST \"$ANTHROPIC_BASE_URL/v1/messages\" -H 'content-type: application/json' --data-binary @\"$HOME/$IN\"";

/// A Messages API answer that stops for good.
fn answer() -> ProviderResult {
    let body = json!({
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    ProviderResult::Response(serde_json::to_vec(&body).unwrap(), Default::default())
}

struct World {
    dir: TempDir,
    task: TaskId,
    db: Db,
    blobs: BlobStore,
    counts: ExecCounts,
    provider: FakeProvider,
}

fn contract_json(caps: &[&str], model_requests: u32, deadline_seconds: u32) -> String {
    let caps = serde_json::to_string(caps).unwrap();
    format!(
        r#"{{
        "goal": "fix the parser",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "python-stdlib-v1",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": {model_requests},
            "max_output_tokens_per_request": 1000,
            "tool_actions": 10,
            "deadline_seconds": {deadline_seconds},
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }}
    }}"#
    )
}

impl World {
    fn new(
        caps: &[&str],
        model_requests: u32,
        deadline_seconds: u32,
        responses: Vec<ProviderResult>,
    ) -> World {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let json = contract_json(caps, model_requests, deadline_seconds);
        let contract = Contract::parse(&json).unwrap();
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let task = db
            .create_task(&contract, &Digest::of(json.as_bytes()))
            .unwrap();
        db.append_audit(
            &task,
            "Submitted",
            &json!({"model_policy_version": 1, "model_limits_version": 1, "model": "fake:test"}),
        )
        .unwrap();
        db.approve_task(&task).unwrap();
        World {
            dir,
            task,
            db,
            blobs,
            counts: ExecCounts::default(),
            provider: FakeProvider::scripted(responses),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The snapshot's digest: the workspace before any change.
    fn base(&self) -> Digest {
        workspace_digest(&self.path("snapshot")).unwrap()
    }

    /// Writes a CLI script and returns the argv that runs it.
    fn cli(&self, script: &str) -> Vec<String> {
        let path = self.path("cli.sh");
        fs::write(&path, script).unwrap();
        vec!["/bin/sh".into(), path.display().to_string()]
    }

    fn events(&self) -> Vec<StoredEvent> {
        self.db.events(&self.task).unwrap()
    }

    fn count(&self, event_type: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.event_type == event_type)
            .count()
    }

    /// `Denied` audits with `reason`.
    fn denials(&self, reason: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "Denied" && e.payload["reason"] == reason)
            .map(|e| e.payload)
            .collect()
    }

    /// The effects of a kind, in intent order (`kind` is the variant name).
    fn effects(&self, kind: &str) -> Vec<EffectRecord> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "EffectIntended" && kind_of(&e.payload) == kind)
            .map(|e| {
                let id = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
                self.db.effect(&id).unwrap()
            })
            .collect()
    }

    /// The variant names of the intended effects, in order.
    fn intended_kinds(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "EffectIntended")
            .map(|e| kind_of(&e.payload).to_string())
            .collect()
    }

    fn failed_reason(&self) -> Option<String> {
        self.events()
            .into_iter()
            .rev()
            .find(|e| e.event_type == "Failed")
            .map(|e| e.payload["Failed"]["reason"].as_str().unwrap().to_string())
    }

    /// The session agent for `argv`, asking for the model `claude-opus-5-5`.
    fn session(&self, argv: Vec<String>) -> SessionAgent {
        SessionAgent::new(argv, vec![], "claude-opus-5-5")
    }

    /// Runs `agent` over the task: the session effects go to the fake guest's session worker,
    /// the model calls to the fake provider.
    async fn run(&self, agent: &mut impl Agent) -> TaskState {
        let worker = WorkerConfig::Firecracker(fake_firecracker_config(self.dir.path()));
        let jobs = supervised(
            &self.path("jobs"),
            worker,
            &self.counts,
            None,
            &[("AGENTOS_TEST_WORKERS", "1")],
        );
        let exec = routing_over(
            self.dir.path(),
            jobs,
            Some(Box::new(self.provider.clone_handle())),
            &self.counts,
            None,
        );
        run_task(&self.db, &self.blobs, &exec, agent, &self.task)
            .await
            .unwrap()
    }
}

/// The variant name of an effect payload's kind (`"ApplyPatch"` for `{"ApplyPatch": {..}}`).
fn kind_of(payload: &Value) -> &str {
    payload["kind"]
        .as_str()
        .or_else(|| {
            payload["kind"]
                .as_object()
                .and_then(|m| m.keys().next())
                .map(String::as_str)
        })
        .unwrap_or("")
}

/// The parser fix, written by the CLI as the file it wants (the same change as
/// `fixtures/parser-repo.fix.patch`).
const FIX: &str = r##"cat > src/parser.py <<'PY'
def parse_kv(text: str) -> dict:
    """Parse 'key = value' lines into a dict, skipping blanks and '#' comments."""
    result = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        result[key.strip()] = value.strip()
    return result
PY
"##;

/// One model call of a CLI script, its body sent from a file.
fn call_script(n: u32, body: &str) -> String {
    format!(
        "printf '%s' '{body}' > \"$HOME/req{n}.json\"\nIN=req{n}.json\nOUT=reply{n}.json\n{CURL}\n"
    )
}

/// A CLI that makes two model calls, and writes the fix between them.
fn two_calls_and_a_fix() -> String {
    let first = r#"{"model":"m","max_tokens":8,"stream":false,"messages":[]}"#;
    let again = r#"{"model":"m","max_tokens":8,"stream":false,"messages":[{"role":"user","content":"again"}]}"#;
    format!(
        "set -eu\n{}{FIX}{}",
        call_script(1, first),
        call_script(2, again)
    )
}

/// One call with a body the runner must rewrite: a huge `max_tokens`, beta-only fields, and
/// the CLI's own model.
const BIG_REQUEST: &str = r#"{"model":"claude-opus-4","max_tokens":128000,"stream":true,"context_management":{"edits":[]},"safeguards":{},"output_config":{},"messages":[{"role":"user","content":"hi"}]}"#;

fn one_call(body: &str, after: &str) -> String {
    format!(
        "set -eu\n\
         printf '%s' '{body}' > \"$HOME/req1.json\"\n\
         IN=req1.json\nOUT=reply1.json\n{CURL}\n{after}\n"
    )
}

fn two_calls(body: &str) -> String {
    format!(
        "set -eu\n\
         printf '%s' '{body}' > \"$HOME/req1.json\"\n\
         IN=req1.json\nOUT=reply1.json\n{CURL}\n\
         printf '%s' '{body}' > \"$HOME/req2.json\"\n\
         IN=req2.json\nOUT=reply2.json\n{CURL}\n"
    )
}

#[tokio::test]
async fn a_session_makes_two_journaled_model_calls_and_its_patch_is_verified() {
    let w = World::new(CAPS, 4, 600, vec![answer(), answer()]);
    let argv = w.cli(&two_calls_and_a_fix());
    let mut agent = w.session(argv);
    assert_eq!(
        w.run(&mut agent).await,
        TaskState::Succeeded,
        "{:?}",
        w.failed_reason()
    );
    assert_eq!(w.provider.calls(), 2);
    let calls = w.effects("ModelCall");
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| c.state == EffectState::Completed));
    assert_eq!(w.count("SessionModelCall"), 2);
    let kinds = w.intended_kinds();
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("RunAgentSession") < at("ApplyPatch"), "{kinds:?}");
    assert!(at("ApplyPatch") < at("RunVerification"), "{kinds:?}");
    let task = w.db.task(&w.task).unwrap();
    assert_eq!(task.verified_digest, Some(task.workspace_digest));
}

#[tokio::test]
async fn the_model_budget_stops_a_session_that_calls_past_it_and_kills_the_cli() {
    let w = World::new(CAPS, 1, 600, vec![answer()]);
    let body = r#"{"model":"m","max_tokens":8,"messages":[]}"#;
    let mut agent = w.session(w.cli(&two_calls(body)));
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(w.failed_reason().as_deref(), Some("budget exhausted"));
    assert_eq!(
        w.provider.calls(),
        1,
        "the second call never reached the provider"
    );
    assert!(
        processes_of_home(w.dir.path()).is_empty(),
        "the CLI was killed and reaped"
    );
}

#[tokio::test]
async fn a_task_without_model_request_sends_nothing_to_the_provider() {
    let caps: Vec<&str> = CAPS
        .iter()
        .copied()
        .filter(|c| *c != "model.request")
        .collect();
    let w = World::new(&caps, 4, 600, vec![answer()]);
    let body = r#"{"model":"m","max_tokens":8,"messages":[]}"#;
    let mut agent = w.session(w.cli(&one_call(body, "")));
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("capability model.request not granted")
    );
    assert_eq!(w.provider.calls(), 0);
    assert_eq!(w.denials("CapabilityDenied").len(), 1);
    assert!(processes_of_home(w.dir.path()).is_empty());
}

#[tokio::test]
async fn a_guest_request_is_clamped_stripped_and_rewritten_before_it_is_journaled() {
    let w = World::new(CAPS, 4, 600, vec![answer()]);
    let mut agent = w.session(w.cli(&one_call(BIG_REQUEST, "")));
    // The CLI changed nothing, so the task ends without a verified success.
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("agent finished without verified success")
    );
    assert_eq!(w.provider.calls(), 1);
    let calls = w.effects("ModelCall");
    let sent: Value =
        serde_json::from_slice(&w.blobs.get(&calls[0].request_digest).unwrap()).unwrap();
    assert_eq!(sent["max_tokens"], 1000, "clamped to the contract's cap");
    assert_eq!(sent["stream"], false);
    assert_eq!(
        sent["model"], "claude-opus-5-5",
        "rewritten to the session's model"
    );
    for field in ["context_management", "safeguards", "output_config"] {
        assert!(sent.get(field).is_none(), "{field} is stripped");
    }
    let audit = w
        .events()
        .into_iter()
        .find(|e| e.event_type == "SessionModelCall")
        .unwrap()
        .payload;
    assert_eq!(audit["requested_model"], "claude-opus-4");
    assert_eq!(audit["model"], "claude-opus-5-5");
    assert_eq!(audit["request"], json!(calls[0].request_digest));
}

#[tokio::test]
async fn a_patch_outside_the_editable_paths_is_denied_and_the_workspace_is_unchanged() {
    let w = World::new(CAPS, 4, 600, vec![]);
    let base = w.base();
    let script = "set -eu\nprintf 'edited\\n' >> README.md\n";
    let mut agent = w.session(w.cli(script));
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(w.denials("PathNotEditable").len(), 1);
    assert_eq!(w.db.task(&w.task).unwrap().workspace_digest, base);
    assert!(!w.intended_kinds().iter().any(|k| k == "ApplyPatch"));
    assert_eq!(w.provider.calls(), 0);
}

#[tokio::test]
async fn a_crafted_patch_into_excluded_or_git_paths_is_denied_by_the_host() {
    for (path, reason) in [
        ("src/__pycache__/x.pyc", "DigestExcludedPath"),
        (".git/hooks/x", "PathNotEditable"),
    ] {
        let w = World::new(CAPS, 4, 600, vec![]);
        let base = w.base();
        let argv = w.cli("exit 0\n");
        // The guest drops excluded paths from what it returns, so the host gets them only from
        // an agent that applies a patch of its own after the session.
        let patch = create_patch(path, "x");
        let mut agent = FnAgent(move |obs: &Observation| match obs {
            Observation::Start { .. } => AgentAction::RunSession {
                argv: argv.clone(),
                env: vec![],
                model: "claude-opus-5-5".into(),
            },
            Observation::SessionEnded { .. } => AgentAction::ApplyPatch(patch.clone()),
            _ => AgentAction::Finish,
        });
        assert_eq!(w.run(&mut agent).await, TaskState::Failed, "{path}");
        assert_eq!(w.denials(reason).len(), 1, "{path}: {:?}", w.events());
        assert_eq!(w.db.task(&w.task).unwrap().workspace_digest, base, "{path}");
    }
}

/// The session's CLI is given the task's time less a margin, so a CLI that never ends is ended
/// by its own timeout: the guest kills it, the agent sees the session timed out, and no process
/// is left behind.
#[tokio::test]
async fn a_session_that_outlives_its_time_is_ended_with_the_cli_killed_and_reported() {
    let w = World::new(CAPS, 4, 15, vec![]);
    let started = Instant::now();
    let mut agent = w.session(w.cli("set -eu\nsleep 600\n"));
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("agent finished without verified success")
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    let ended = w
        .events()
        .into_iter()
        .filter(|e| e.event_type == "AgentTurn")
        .find(|e| e.payload["observation"].get("SessionEnded").is_some())
        .expect("the agent saw the session end");
    assert_eq!(
        ended.payload["observation"]["SessionEnded"]["timed_out"],
        true
    );
    assert!(
        processes_of_home(w.dir.path()).is_empty(),
        "the CLI was killed"
    );
}

#[tokio::test]
async fn a_task_without_agent_session_fails_before_any_session_job_exists() {
    let caps: Vec<&str> = CAPS
        .iter()
        .copied()
        .filter(|c| *c != "agent.session")
        .collect();
    let w = World::new(&caps, 4, 600, vec![answer()]);
    let mut agent = w.session(w.cli(&one_call(r#"{"model":"m","messages":[]}"#, "")));
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("capability agent.session not granted")
    );
    assert!(w.effects("RunAgentSession").is_empty());
    assert_eq!(w.provider.calls(), 0);
    let denied = w.denials("CapabilityDenied");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0]["action"], "RunSession");
    let jobs = w.path("jobs");
    for entry in fs::read_dir(&jobs).unwrap() {
        let request: Value =
            serde_json::from_slice(&fs::read(entry.unwrap().path().join("request.json")).unwrap())
                .unwrap();
        assert!(
            request["kind"].get("RunAgentSession").is_none(),
            "no session job"
        );
    }
}
