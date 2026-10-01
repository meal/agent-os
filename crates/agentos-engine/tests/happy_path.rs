mod common;

use agentos_core::effect::{EffectKind, EffectState, Outcome};
use agentos_core::ids::Digest;
use agentos_core::budget::Reservation;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{AgentAction, FakeAgent, Observation};
use agentos_engine::runner::run_task;
use agentos_engine::workspace::workspace_digest;
use agentos_store::db::DbError;
use std::sync::Mutex;

use agentos_engine::executor::EffectRequest;
use common::{comment_patch, create_patch, edit_patch, fix_patch, Env, FnAgent, HookExec};

const NO_VERIFY: &[&str] = &["snapshot.read", "workspace.apply_patch"];
const NO_SNAPSHOT: &[&str] = &["workspace.apply_patch", "verification.run"];

async fn run(env: &Env, agent: &mut impl agentos_engine::agent::Agent) -> TaskState {
    run_task(&env.db, &env.blobs, &env.exec, agent, &env.task).await.unwrap()
}

fn assert_subsequence(haystack: &[String], needles: &[&str]) {
    let mut it = haystack.iter();
    for n in needles {
        assert!(it.any(|h| h == n), "missing {n:?} in order within {haystack:?}");
    }
}

// (a)
#[tokio::test]
async fn happy_path_succeeds_with_patch_blob_and_evidence_for_final_workspace() {
    let env = Env::new(10);
    let profile_digest = workspace_digest(&env.profile_dir()).unwrap();
    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);

    let task = env.db.task(&env.task).unwrap();
    let ws = env.ws_digest();
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.workspace_digest, ws);
    assert_eq!(task.verified_digest, Some(ws));
    assert_ne!(ws, base, "the patch changed the workspace");

    let types = env.event_types();
    assert_subsequence(
        &types,
        &[
            "TaskCreated", "Started",
            "EffectIntended", "ActionUsed", "EffectDispatched", "ArtifactRegistered", "EffectCompleted", "WorkspaceUpdated",
            "EffectIntended", "ActionUsed", "EffectDispatched", "EffectCompleted", "WorkspaceUpdated",
            "VerifyStarted", "EffectIntended", "EffectDispatched", "EffectCompleted", "VerifyPassed",
        ],
    );
    assert_eq!(env.count("EffectFailed"), 0);
    assert_eq!(env.count("Denied"), 0);

    // The snapshot replaced the placeholder base digest with the real one.
    let updates: Vec<Digest> = env
        .events()
        .iter()
        .filter(|e| e.event_type == "WorkspaceUpdated")
        .map(|e| serde_json::from_value(e.payload["WorkspaceUpdated"]["digest"].clone()).unwrap())
        .collect();
    assert_eq!(updates, vec![base, ws]);

    for kind in ["ReadSnapshot", "ApplyPatch", "RunVerification"] {
        let effects = env.effects(kind);
        assert_eq!(effects.len(), 1, "{kind}");
        assert_eq!(effects[0].state, EffectState::Completed, "{kind}");
        assert_eq!(effects[0].lease_generation, 1, "{kind}");
    }

    let snapshot = &env.effects("ReadSnapshot")[0];
    let manifest = env.blob_json(&snapshot.result_digest.unwrap());
    assert_eq!(manifest["workspace_digest"], base.to_string());
    assert!(manifest["files"].as_array().unwrap().iter().any(|f| f == "src/parser.py"));

    let patch = &env.effects("ApplyPatch")[0];
    let patch_digest = Digest::of(fix_patch().as_bytes());
    assert_eq!(patch.request_digest, patch_digest);
    assert_eq!(env.blobs.get(&patch_digest).unwrap(), fix_patch().into_bytes());
    assert!(env.db.referenced_blobs().unwrap().contains(&patch_digest));

    let verify = &env.effects("RunVerification")[0];
    let evidence = env.blob_json(&verify.result_digest.unwrap());
    assert_eq!(evidence["exit_code"], 0);
    assert_eq!(evidence["workspace_digest"], ws.to_string());
    assert_eq!(evidence["profile_id"], "parser-checks-v1");
    assert_eq!(evidence["profile_digest"], profile_digest.to_string());
    assert!(evidence["stdout"].as_str().unwrap().contains("10/10 checks passed"));

    let usage = env.db.usage_summary(&env.task).unwrap();
    assert_eq!(usage.settled_tool_actions, 2);
    assert_eq!((usage.reserved_tool_actions, usage.uncertain_tool_actions), (0, 0));
    assert_eq!(task.actions_used, 2);
    assert!(env.db.outstanding_effects(&env.task).unwrap().is_empty());

    let obs = agent.observations();
    assert!(matches!(&obs[0], Observation::Start { workspace, files } if *workspace == base && files.contains(&"src/parser.py".to_string())));
    assert_eq!(obs[1], Observation::PatchApplied { workspace: ws });
    assert_eq!(obs.len(), 2, "the runner stops as soon as the task succeeds");
}

// (b)
#[tokio::test]
async fn patch_touching_tests_is_denied_without_effect_or_action_and_task_still_succeeds() {
    let env = Env::new(10);
    let bad = edit_patch("tests/test_parser.py", "import unittest", "import unittest  # weakened");
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(bad.clone()),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);

    let denials = env.denials("PathNotEditable");
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0]["action"], "ApplyPatch");
    assert_eq!(denials[0]["paths"], serde_json::json!(["tests/test_parser.py"]));
    assert_eq!(denials[0]["request_digest"], Digest::of(bad.as_bytes()).to_string());

    let patches = env.effects("ApplyPatch");
    assert_eq!(patches.len(), 1, "no effect for the denied patch");
    assert_eq!(patches[0].request_digest, Digest::of(fix_patch().as_bytes()));
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 2);
    assert_eq!(env.db.usage_summary(&env.task).unwrap().settled_tool_actions, 2);
    assert!(matches!(&agent.observations()[1], Observation::PatchRejected { reason } if reason.contains("tests/test_parser.py")));
    let original = std::fs::read(env.snapshot_dir().join("tests/test_parser.py")).unwrap();
    assert_eq!(std::fs::read(env.ws().join("tests/test_parser.py")).unwrap(), original);
}

#[tokio::test]
async fn unparseable_and_traversal_patches_are_denied() {
    let env = Env::new(10);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch("this is not a patch\n".into()),
        AgentAction::ApplyPatch(edit_patch("src/../tests/test_parser.py", "import unittest", "x")),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);
    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);
    assert_eq!(env.denials("InvalidPatch").len(), 1);
    assert_eq!(env.denials("PathNotEditable").len(), 1);
    assert_eq!(env.effects("ApplyPatch").len(), 1);
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 2);
}

// (c) through the runner: the workspace moved on while the agent was deciding.
#[tokio::test]
async fn stale_base_returns_version_conflict_and_changes_nothing() {
    let env = Env::new(10);
    let writer = env.second_db();
    let task = env.task.clone();
    let concurrent = Digest::of(b"someone else's workspace");
    let mut seen = Vec::new();
    let mut agent = FnAgent(|obs: &Observation| {
        seen.push(obs.clone());
        match obs {
            Observation::Start { .. } => {
                writer.append(&task, &TaskEvent::WorkspaceUpdated { digest: concurrent }).unwrap();
                AgentAction::ApplyPatch(fix_patch())
            }
            _ => AgentAction::Finish,
        }
    });

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    assert_eq!(seen[1], Observation::VersionConflict { expected: base, actual: concurrent });
    let conflicts = env.denials("VersionConflict");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["expected"], base.to_string());
    assert_eq!(conflicts[0]["actual"], concurrent.to_string());
    assert!(env.effects("ApplyPatch").is_empty());
    assert_eq!(env.ws_digest(), base, "workspace files untouched");
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 1);
}

// (c) at the store: an intent built against an outdated workspace version.
#[tokio::test]
async fn store_rejects_intent_against_outdated_workspace() {
    let env = Env::new(10);
    // Apply the fix, then pause so the task stays live for the direct store call.
    let writer = env.second_db();
    let task = env.task.clone();
    let mut pausing = FnAgent(|obs: &Observation| match obs {
        Observation::Start { .. } => AgentAction::ApplyPatch(fix_patch()),
        _ => {
            writer.append(&task, &TaskEvent::Paused).unwrap();
            AgentAction::Finish
        }
    });
    assert_eq!(run(&env, &mut pausing).await, TaskState::Paused);
    env.db.append(&env.task, &TaskEvent::Resumed).unwrap();

    let stale = workspace_digest(&env.snapshot_dir()).unwrap();
    let before = env.db.task(&env.task).unwrap();
    assert_ne!(before.workspace_digest, stale);
    let kind = EffectKind::ApplyPatch { expected_base: stale };
    let err = env
        .db
        .record_intent(&env.task, kind.clone(), Digest::of(b"p2"), &stale, Reservation::for_kind(&kind, 0))
        .unwrap_err();
    assert!(matches!(err, DbError::VersionConflict { expected, actual } if expected == stale && actual == before.workspace_digest));
    assert_eq!(env.denials("VersionConflict").len(), 1);
    let after = env.db.task(&env.task).unwrap();
    assert_eq!(after, before);
    assert_eq!(env.effects("ApplyPatch").len(), 1);
}

// (d)
#[tokio::test]
async fn protected_profile_cannot_be_changed_by_the_agent() {
    let env = Env::new(10);
    let profile_digest = workspace_digest(&env.profile_dir()).unwrap();
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(create_patch("check_parser.py", "import sys; sys.exit(0)")),
        AgentAction::ApplyPatch(edit_patch("../profile/profile.json", "x", "y")),
        AgentAction::ApplyPatch(edit_patch("src/../../profile/check_parser.py", "x", "y")),
        // Allowed: a file of the same name inside the workspace does not affect the check.
        AgentAction::ApplyPatch(create_patch("src/check_parser.py", "import sys; sys.exit(0)")),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.denials("PathNotEditable").len(), 3);
    assert_eq!(env.effects("ApplyPatch").len(), 1);
    assert!(env.ws().join("src/check_parser.py").is_file());
    let verify = env.effects("RunVerification");
    assert_eq!(verify.len(), 1);
    let evidence = env.blob_json(&verify[0].result_digest.unwrap());
    assert_eq!(evidence["profile_digest"], profile_digest.to_string());
    assert_ne!(evidence["exit_code"], 0);
    assert_eq!(workspace_digest(&env.profile_dir()).unwrap(), profile_digest);
    assert_eq!(env.count("VerifyFailed"), 1);
    assert_eq!(env.count("VerifyPassed"), 0);
}

// (d) a patch whose code rewrites the check at run time is caught and does not persist.
#[tokio::test]
async fn code_that_tampers_with_the_profile_during_verification_cannot_pass() {
    let env = Env::new(10);
    let profile_digest = workspace_digest(&env.profile_dir()).unwrap();
    let tamper = "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1 +1,4 @@\n+import pathlib\n+for _p in pathlib.Path('.').glob('*'):\n+    _p.write_text('import sys\\nsys.exit(0)\\n')\n def parse_kv(text: str) -> dict:\n";
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::ApplyPatch(tamper.into()),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.effects("ApplyPatch").len(), 2);
    assert_eq!(env.count("VerifyPassed"), 0);
    assert_eq!(workspace_digest(&env.profile_dir()).unwrap(), profile_digest);
    let verify = &env.effects("RunVerification")[0];
    assert_eq!(verify.state, EffectState::Failed);
    let failure = env.blob_json(&verify.result_digest.unwrap());
    assert!(failure["reason"].as_str().unwrap().contains("profile"), "{failure}");
}

// (e)
#[tokio::test]
async fn non_fixing_patch_fails_verification_and_finish_fails_the_task() {
    let env = Env::new(10);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(comment_patch()),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.count("VerifyFailed"), 1);
    assert_eq!(env.count("VerifyPassed"), 0);
    let task = env.db.task(&env.task).unwrap();
    assert_eq!(task.verified_digest, None);
    let failed = env.events().into_iter().find(|e| e.event_type == "Failed").unwrap();
    assert_eq!(failed.payload["Failed"]["reason"], "agent finished without verified success");
    assert!(matches!(&agent.observations()[2], Observation::Verification { passed: false, .. }));
    let evidence = env.blob_json(&env.effects("RunVerification")[0].result_digest.unwrap());
    assert_eq!(evidence["exit_code"], 1);
}

#[tokio::test]
async fn failed_verification_with_exhausted_actions_fails_the_task() {
    // ReadSnapshot + one patch use both actions; the failed check then fails the task.
    let env = Env::new(2);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(comment_patch()),
        AgentAction::Verify,
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.count("VerifyFailed"), 1);
    assert_eq!(env.count("VerifyPassed"), 0);
    assert_eq!(env.effects("ApplyPatch").len(), 1);
    assert_eq!(agent.observations().len(), 2, "the reducer failed the task; no further turns");
}

#[tokio::test]
async fn exhausted_tool_actions_fail_the_task_with_budget_exhausted() {
    let env = Env::new(2);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(comment_patch()),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    let failed = env.events().into_iter().find(|e| e.event_type == "Failed").unwrap();
    assert_eq!(failed.payload["Failed"]["reason"], "budget exhausted");
    assert_eq!(env.effects("ApplyPatch").len(), 1);
    assert_eq!(env.count("VerifyStarted"), 0);
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 2);
}

#[tokio::test]
async fn retrying_a_patch_that_failed_to_apply_is_rejected_again_without_error() {
    let env = Env::new(10);
    let wrong = edit_patch("src/parser.py", "no such line", "x");
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(wrong.clone()),
        AgentAction::ApplyPatch(wrong),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);

    let obs = agent.observations();
    assert!(matches!(obs[1], Observation::PatchRejected { .. }), "{:?}", obs[1]);
    assert!(matches!(obs[2], Observation::PatchRejected { .. }), "{:?}", obs[2]);
    let patches = env.effects("ApplyPatch");
    assert_eq!(patches.len(), 2, "the retry reuses the failed effect");
    assert_eq!(patches[0].state, EffectState::Failed);
    let failure = env.blob_json(&patches[0].result_digest.unwrap());
    assert_eq!(failure["outcome"], "failure");
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 3);
}

#[tokio::test]
async fn applying_the_fix_twice_rejects_the_second_apply() {
    // The second apply fails (already applied) and is rejected.
    let env = Env::new(10);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);
    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);
    let patches = env.effects("ApplyPatch");
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[1].state, EffectState::Failed);
    assert!(matches!(agent.observations()[2], Observation::PatchRejected { .. }));
}

#[tokio::test]
async fn run_task_on_a_terminal_task_returns_without_new_events() {
    let env = Env::new(10);
    env.db.append(&env.task, &TaskEvent::Failed { reason: "earlier".into() }).unwrap();
    let before = env.events().len();
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.events().len(), before);
    assert!(agent.observations().is_empty());
}

#[tokio::test]
async fn cancel_requested_before_the_loop_cancels_without_acting() {
    let env = Env::new(10);
    env.db.append(&env.task, &TaskEvent::CancelRequested).unwrap();
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(run(&env, &mut agent).await, TaskState::Cancelled);

    assert_eq!(env.event_types(), vec!["TaskCreated", "CancelRequested", "CancelCompleted"]);
    assert!(agent.observations().is_empty());
    assert!(!env.ws().exists());
}

#[tokio::test]
async fn cancel_requested_while_the_agent_decides_drops_the_action() {
    let env = Env::new(10);
    let writer = env.second_db();
    let task = env.task.clone();
    let mut agent = FnAgent(|_: &Observation| {
        writer.append(&task, &TaskEvent::CancelRequested).unwrap();
        AgentAction::ApplyPatch(fix_patch())
    });

    assert_eq!(run(&env, &mut agent).await, TaskState::Cancelled);

    assert!(env.effects("ApplyPatch").is_empty());
    assert_eq!(env.ws_digest(), workspace_digest(&env.snapshot_dir()).unwrap());
}

#[tokio::test]
async fn paused_task_resumes_and_finishes() {
    let env = Env::new(10);
    let writer = env.second_db();
    let task = env.task.clone();
    let mut first = FnAgent(|_: &Observation| {
        writer.append(&task, &TaskEvent::Paused).unwrap();
        AgentAction::ApplyPatch(fix_patch())
    });

    assert_eq!(run(&env, &mut first).await, TaskState::Paused);
    assert_eq!(env.effects("ReadSnapshot").len(), 1);
    assert!(env.effects("ApplyPatch").is_empty());

    // A paused task is left alone.
    let mut idle = FakeAgent::scripted(vec![]);
    assert_eq!(run(&env, &mut idle).await, TaskState::Paused);
    assert!(idle.observations().is_empty());

    env.db.append(&env.task, &TaskEvent::Resumed).unwrap();
    let mut second = FakeAgent::from_fixture_patch(fix_patch());
    assert_eq!(run(&env, &mut second).await, TaskState::Succeeded);

    assert_eq!(env.effects("ReadSnapshot").len(), 1, "the snapshot is not taken again");
    assert_eq!(env.count("Started"), 1);
    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    assert!(matches!(&second.observations()[0], Observation::Start { workspace, files } if *workspace == base && !files.is_empty()));
    let task = env.db.task(&env.task).unwrap();
    assert_eq!(task.verified_digest, Some(env.ws_digest()));
    assert_eq!(task.actions_used, 2);
}

#[tokio::test]
async fn an_agent_that_never_finishes_is_stopped() {
    let env = Env::new(3);
    let bad = edit_patch("tests/test_parser.py", "a", "b");
    let mut agent = FnAgent(|_: &Observation| AgentAction::ApplyPatch(bad.clone()));

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    let failed = env.events().into_iter().find(|e| e.event_type == "Failed").unwrap();
    assert_eq!(failed.payload["Failed"]["reason"], "agent turn limit exceeded");
}

#[tokio::test]
async fn effect_failure_receipts_are_published() {
    let env = Env::new(10);
    let mut agent = FakeAgent::scripted(vec![AgentAction::ApplyPatch(edit_patch("src/parser.py", "nope", "x"))]);
    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);
    let patch = &env.effects("ApplyPatch")[0];
    assert_eq!(patch.state, EffectState::Failed);
    let completed = env.events().into_iter().find(|e| e.event_type == "EffectFailed").unwrap();
    let outcome: Outcome = serde_json::from_value(completed.payload["outcome"].clone()).unwrap();
    assert!(matches!(outcome, Outcome::Failure(_)));
    assert!(env.blobs.get(&patch.result_digest.unwrap()).is_ok());
}

#[tokio::test]
async fn missing_snapshot_capability_fails_the_task() {
    let env = Env::with_caps(10, NO_SNAPSHOT);
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    let failed = env.events().into_iter().find(|e| e.event_type == "Failed").unwrap();
    assert_eq!(failed.payload["Failed"]["reason"], "capability snapshot.read not granted");
    assert!(env.effects("ReadSnapshot").is_empty());
    assert!(agent.observations().is_empty());
    assert!(!env.ws().exists());
    // Calling again is a no-op on the terminal task.
    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);
}

#[tokio::test]
async fn missing_verification_capability_is_denied_without_entering_verifying() {
    let env = Env::with_caps(10, NO_VERIFY);
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
        AgentAction::Finish,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Failed);

    assert_eq!(env.count("VerifyStarted"), 0);
    assert!(env.effects("RunVerification").is_empty());
    let denials = env.denials("CapabilityDenied");
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0]["action"], "Verify");
    assert_eq!(denials[0]["capability"], "verification.run");
    assert!(matches!(
        &agent.observations()[2],
        Observation::Verification { passed: false, summary } if summary.contains("verification.run")
    ));
    assert!(env.db.outstanding_effects(&env.task).unwrap().is_empty());
}

#[tokio::test]
async fn patch_writing_digest_ignored_paths_is_denied_by_the_broker() {
    let env = Env::new(10);
    let sneaky = create_patch("src/__pycache__/helper.py", "RESULT = True");
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(sneaky.clone()),
        AgentAction::ApplyPatch(create_patch("src/stale.pyc", "x")),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);

    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);

    let denials = env.denials("DigestExcludedPath");
    assert_eq!(denials.len(), 2);
    assert_eq!(denials[0]["action"], "ApplyPatch");
    assert_eq!(denials[0]["paths"], serde_json::json!(["src/__pycache__/helper.py"]));
    assert_eq!(denials[0]["request_digest"], Digest::of(sneaky.as_bytes()).to_string());
    assert_eq!(env.effects("ApplyPatch").len(), 1, "no effect for the denied patches");
    assert_eq!(env.db.task(&env.task).unwrap().actions_used, 2);
    assert!(!env.ws().join("src/__pycache__").exists());
}

fn kind_is(req: &EffectRequest, tag: &str) -> bool {
    req.kind.tag() == tag
}

#[tokio::test]
async fn pause_while_a_patch_is_in_flight_keeps_the_workspace_digest_current() {
    let env = Env::new(10);
    let writer = Mutex::new(env.second_db());
    let task = env.task.clone();
    let exec = HookExec {
        inner: env.fixture_exec(),
        before: |req: &EffectRequest| {
            if kind_is(req, "apply_patch") {
                writer.lock().unwrap().append(&task, &TaskEvent::Paused).unwrap();
            }
        },
        after: |_: &EffectRequest| {},
    };
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task).await.unwrap();

    assert_eq!(state, TaskState::Paused);
    let paused = env.db.task(&env.task).unwrap();
    assert_eq!(paused.state, TaskState::Paused);
    assert_eq!(paused.workspace_digest, env.ws_digest(), "the completed patch updated the digest while paused");
    assert_eq!(env.effects("ApplyPatch")[0].state, EffectState::Completed);
    assert_eq!(env.count("TaskEventRejected"), 0);

    env.db.append(&env.task, &TaskEvent::Resumed).unwrap();
    let mut resumed = FakeAgent::scripted(vec![AgentAction::Verify, AgentAction::Finish]);
    assert_eq!(run(&env, &mut resumed).await, TaskState::Succeeded);
    assert!(matches!(&resumed.observations()[0], Observation::Start { workspace, .. } if *workspace == env.ws_digest()));
    let done = env.db.task(&env.task).unwrap();
    assert_eq!(done.verified_digest, Some(env.ws_digest()));
}

#[tokio::test]
async fn pause_while_the_snapshot_is_in_flight_records_the_real_base() {
    let env = Env::new(10);
    let writer = Mutex::new(env.second_db());
    let task = env.task.clone();
    let exec = HookExec {
        inner: env.fixture_exec(),
        before: |req: &EffectRequest| {
            if kind_is(req, "read_snapshot") {
                writer.lock().unwrap().append(&task, &TaskEvent::Paused).unwrap();
            }
        },
        after: |_: &EffectRequest| {},
    };
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task).await.unwrap();

    assert_eq!(state, TaskState::Paused);
    assert!(agent.observations().is_empty());
    let base = workspace_digest(&env.snapshot_dir()).unwrap();
    assert_eq!(env.db.task(&env.task).unwrap().workspace_digest, base);

    env.db.append(&env.task, &TaskEvent::Resumed).unwrap();
    assert_eq!(run(&env, &mut agent).await, TaskState::Succeeded);
    assert_eq!(env.effects("ReadSnapshot").len(), 1);
    assert_eq!(env.db.task(&env.task).unwrap().verified_digest, Some(env.ws_digest()));
}

// (f) through the run loop: a symlink appears in the live workspace while the patch runs.
#[tokio::test]
async fn run_loop_rejects_a_patch_through_a_live_symlink_and_still_succeeds() {
    let env = Env::new(10);
    let outside = env.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let ws = env.ws();
    let escape = create_patch("src/escape/pwned.py", "owned");
    let link = ws.join("src/escape");
    let exec = HookExec {
        inner: env.fixture_exec(),
        before: |req: &EffectRequest| {
            if req.payload == escape.as_bytes() {
                std::os::unix::fs::symlink(&outside, &link).unwrap();
            }
        },
        after: |req: &EffectRequest| {
            if req.payload == escape.as_bytes() {
                std::fs::remove_file(&link).unwrap();
            }
        },
    };
    let mut agent = FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(escape.clone()),
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ]);

    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task).await.unwrap();

    assert_eq!(state, TaskState::Succeeded);
    assert!(matches!(&agent.observations()[1], Observation::PatchRejected { reason } if reason.contains("symlink")));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing written outside the workspace");
    let patches = env.effects("ApplyPatch");
    assert_eq!((patches[0].state, patches[1].state), (EffectState::Failed, EffectState::Completed));
}
