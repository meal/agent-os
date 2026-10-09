//! A guest agent session through the real runner: the CLI runs as a session job over the fake
//! guest, its model requests are served through the runner as journaled `ModelCall`s (to a
//! fake provider), and its patch goes through `ApplyPatch` and the protected verification.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::{Agent, AgentAction, Observation, SessionAgent};
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::job::{JobDir, WorkerConfig};
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::{BoxFuture, ModelProvider, ProviderResult};
use agentos_engine::runner::{EngineError, run_task_with};
use agentos_engine::supervised::ExecCounts;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use common::{
    CURL, FnAgent, answer, assert_no_process_survives, copy_dir, create_patch,
    fake_firecracker_config, fixtures, routing_over, supervised, two_calls_and_a_fix,
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

    /// A restarted controller: every database and blob store handle is reopened from disk.
    fn reopen(&mut self) {
        self.db = Db::open(&self.path("agentos.db")).unwrap();
        self.blobs = BlobStore::open(self.path("blobs")).unwrap();
    }

    /// Runs `agent` over the task with `opts` (a crash hook makes it a kill at that point).
    /// The session effects go to the fake guest's session worker, the model calls to the fake
    /// provider.
    async fn run_with(
        &self,
        agent: &mut impl Agent,
        opts: &RunOptions,
    ) -> Result<TaskState, EngineError> {
        self.run_via(agent, opts, Box::new(self.provider.clone_handle()))
            .await
    }

    /// [`World::run_with`], with the model calls answered by `provider`.
    async fn run_via(
        &self,
        agent: &mut impl Agent,
        opts: &RunOptions,
        provider: Box<dyn ModelProvider>,
    ) -> Result<TaskState, EngineError> {
        let worker = WorkerConfig::Firecracker(fake_firecracker_config(self.dir.path()));
        let jobs = supervised(
            &self.path("jobs"),
            worker,
            &self.counts,
            None,
            &[("AGENTOS_TEST_WORKERS", "1")],
        );
        let exec = routing_over(self.dir.path(), jobs, Some(provider), &self.counts, None);
        run_task_with(&self.db, &self.blobs, &exec, agent, &self.task, opts).await
    }

    /// Runs `agent` over the task to its end (no crash).
    async fn run(&self, agent: &mut impl Agent) -> TaskState {
        self.run_with(agent, &RunOptions::default()).await.unwrap()
    }
}

/// The fake provider, answering only after `wait`: a model call that is still in flight when
/// its session has already ended.
struct SlowProvider {
    wait: Duration,
    inner: FakeProvider,
}

impl ModelProvider for SlowProvider {
    fn complete<'a>(&'a self, body: &'a [u8]) -> BoxFuture<'a, ProviderResult> {
        Box::pin(async move {
            tokio::time::sleep(self.wait).await;
            self.inner.complete(body).await
        })
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
    assert_no_process_survives(w.dir.path());
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
    assert_no_process_survives(w.dir.path());
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
    assert_no_process_survives(w.dir.path());
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

/// The controller dies after the session finished and its patch was intended: the restarted
/// controller applies that same patch once, verifies, and never runs the session or a model
/// call again.
#[tokio::test]
async fn a_finished_session_is_not_lost_when_the_controller_dies_at_the_next_intent() {
    let mut w = World::new(CAPS, 4, 600, vec![answer(), answer()]);
    let argv = w.cli(&two_calls_and_a_fix());
    let mut agent = w.session(argv.clone());
    let hook = CrashHook::at(CrashPoint::AfterIntent, "apply_patch");
    let err = w
        .run_with(&mut agent, &RunOptions::crash_with(hook))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Crashed(CrashPoint::AfterIntent)),
        "{err:?}"
    );
    assert_eq!(
        w.effects("RunAgentSession")[0].state,
        EffectState::Completed
    );

    w.reopen();
    let mut agent = w.session(argv);
    assert_eq!(
        w.run(&mut agent).await,
        TaskState::Succeeded,
        "{:?}",
        w.failed_reason()
    );
    let patches = w.effects("ApplyPatch");
    assert_eq!(patches.len(), 1, "the patch was intended once");
    assert_eq!(patches[0].state, EffectState::Completed);
    assert_eq!(w.effects("RunVerification").len(), 1);
    assert_eq!(w.counts.get("run_agent_session"), 1, "never run again");
    assert_eq!(w.counts.get("apply_patch"), 1, "the patch was applied once");
    assert_eq!(w.provider.calls(), 2, "no model call was sent twice");
    assert_eq!(w.effects("RunAgentSession").len(), 1);
    let task = w.db.task(&w.task).unwrap();
    assert_eq!(task.verified_digest, Some(task.workspace_digest));
    assert_no_process_survives(w.dir.path());
}

/// The controller dies after the session effect completed and before the next turn was
/// journaled: the restarted controller rebuilds the session's end from its stored result and
/// carries on; the session is not started again.
#[tokio::test]
async fn a_session_completed_before_the_crash_is_rebuilt_and_not_run_again() {
    let mut w = World::new(CAPS, 4, 600, vec![answer(), answer()]);
    let argv = w.cli(&two_calls_and_a_fix());
    let mut agent = w.session(argv.clone());
    let hook = CrashHook::at(CrashPoint::AfterComplete, "run_agent_session");
    let err = w
        .run_with(&mut agent, &RunOptions::crash_with(hook))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Crashed(CrashPoint::AfterComplete)),
        "{err:?}"
    );
    assert_eq!(
        w.effects("RunAgentSession")[0].state,
        EffectState::Completed
    );

    w.reopen();
    let mut agent = w.session(argv);
    assert_eq!(
        w.run(&mut agent).await,
        TaskState::Succeeded,
        "{:?}",
        w.failed_reason()
    );
    assert_eq!(w.effects("RunAgentSession").len(), 1, "no second session");
    assert_eq!(w.counts.get("run_agent_session"), 1);
    assert_eq!(w.provider.calls(), 2);
    assert_eq!(w.effects("ApplyPatch")[0].state, EffectState::Completed);
    assert_eq!(w.effects("RunVerification").len(), 1);
    assert_no_process_survives(w.dir.path());
}

/// The controller dies while the session runs, just after its first model call was answered
/// (the response is stored, not yet sent to the CLI). The restarted controller ends the task
/// as lost, kills the session's job, never sends the answered call again, and counts it used.
#[tokio::test]
async fn a_session_killed_while_running_ends_the_task_as_lost_and_the_call_is_not_resent() {
    // A short deadline: the failing run must not wait for the session's own lease.
    let mut w = World::new(CAPS, 4, 30, vec![answer()]);
    let body = r#"{"model":"m","max_tokens":8,"messages":[]}"#;
    // The CLI waits for the reply that the dead controller never sends.
    let argv = w.cli(&one_call(body, ""));
    let mut agent = w.session(argv.clone());
    let hook = CrashHook::at(CrashPoint::AfterComplete, "model_call");
    let err = w
        .run_with(&mut agent, &RunOptions::crash_with(hook))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Crashed(CrashPoint::AfterComplete)),
        "{err:?}"
    );
    // The kill is a real one: the session is still open, its job still running.
    assert_eq!(
        w.effects("RunAgentSession")[0].state,
        EffectState::Dispatched
    );
    assert_eq!(w.effects("ModelCall")[0].state, EffectState::Completed);

    w.reopen();
    let mut agent = w.session(argv);
    assert_eq!(w.run(&mut agent).await, TaskState::Failed);
    assert_eq!(
        w.failed_reason().as_deref(),
        Some("agent session lost"),
        "{:?}",
        w.events()
    );
    assert_eq!(
        w.provider.calls(),
        1,
        "the answered call is never sent again"
    );
    assert_eq!(w.counts.get("run_agent_session"), 1, "never run again");
    assert_eq!(w.effects("ModelCall")[0].state, EffectState::Completed);
    assert_eq!(
        w.db.usage_summary(&w.task).unwrap().settled_model_requests,
        1
    );
    // The cancelled job's receipt is not the session's result: the session is lost, not
    // published as a failure (and never run again).
    let decisions: Vec<_> = w
        .events()
        .into_iter()
        .filter(|e| e.event_type == "RecoveryDecision" && e.payload["kind"] == "run_agent_session")
        .map(|e| e.payload["decision"].clone())
        .collect();
    assert_eq!(decisions, vec![json!("Unreconcilable")]);
    assert_eq!(w.effects("RunAgentSession")[0].state, EffectState::Unknown);
    let session = w.effects("RunAgentSession")[0].effect_id.to_string();
    assert!(
        !w.events()
            .iter()
            .any(|e| e.event_type == "EffectFailed" && e.payload["effect_id"] == session.as_str()),
        "the cancelled receipt was not published"
    );
    assert_no_process_survives(w.dir.path());
}

/// The session job ends while its model call is still in flight (here: it is cancelled after
/// the call was sent). The call is answered and settled as any other (it is not left DISPATCHED
/// for a later recovery), and it is sent exactly once.
#[tokio::test]
async fn a_model_call_still_in_flight_when_its_session_ends_is_settled() {
    let w = World::new(CAPS, 4, 600, vec![answer()]);
    // The CLI asks once, then waits; the session is cancelled while the answer is on its way.
    let body = r#"{"model":"m","max_tokens":8,"messages":[]}"#;
    let script = format!(
        "set -eu\nprintf '%s' '{body}' > \"$HOME/req1.json\"\nIN=req1.json\nOUT=reply1.json\n{CURL} &\nsleep 30\n"
    );
    let argv = w.cli(&script);
    let jobs = w.path("jobs");
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        for entry in fs::read_dir(jobs).unwrap().flatten() {
            let job = JobDir::open(&entry.path()).unwrap();
            if job
                .request()
                .is_ok_and(|r| matches!(r.kind, EffectKind::RunAgentSession { .. }))
            {
                job.drop_cancel().unwrap();
            }
        }
    });
    let slow = SlowProvider {
        wait: Duration::from_secs(3),
        inner: w.provider.clone_handle(),
    };
    let mut agent = w.session(argv);
    assert_eq!(
        w.run_via(&mut agent, &RunOptions::default(), Box::new(slow))
            .await
            .unwrap(),
        TaskState::Failed
    );
    // Whether the cancelled job left its receipt before it was killed decides the session's
    // end: a failure receipt (FAILED), or none (UNKNOWN, lost). Both end the task as lost.
    let session = w.effects("RunAgentSession")[0].state;
    match w.failed_reason().as_deref() {
        Some("agent session lost") => assert_eq!(session, EffectState::Unknown),
        Some("agent session failed: agent session cancelled") => {
            assert_eq!(session, EffectState::Failed)
        }
        other => panic!("the session ended as {other:?}"),
    }
    assert_eq!(w.provider.calls(), 1, "the call was sent once");
    assert_eq!(w.effects("ModelCall")[0].state, EffectState::Completed);
    // Only the lost session stays open (UNKNOWN); no model call is left in flight.
    let open = w.db.outstanding_effects(&w.task).unwrap();
    assert!(
        open.iter().all(|e| e.kind.tag() == "run_agent_session"),
        "{open:?}"
    );
    assert_eq!(
        w.db.usage_summary(&w.task).unwrap().settled_model_requests,
        1
    );
    assert_no_process_survives(w.dir.path());
}
