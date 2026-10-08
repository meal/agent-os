//! The analyzer in a task: one `AnalyzeSnapshot` effect after the snapshot and before the
//! agent's first turn, authorized by the broker, advisory (a failure does not stop the task),
//! never a verification, and recovered at every crash point without running again once its
//! outcome is retained.

mod common;

use agentos_core::effect::EffectState;
use agentos_core::state::TaskState;
use agentos_engine::agent::{AgentAction, FakeAgent};
use agentos_engine::analysis::AnalysisExecutor;
use agentos_engine::crash::{CrashHook, CrashPoint, RunOptions};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::runner::{EngineError, run_task_with};
use agentos_engine::supervised::ExecCounts;
use common::{Env, analyzer_contract, component_wasm, fix_patch, routing_over};

fn env(component: &str, mode: Option<&str>) -> Env {
    let env = Env::with_contract(analyzer_contract(10, component));
    if let Some(mode) = mode {
        std::fs::write(env.snapshot_dir().join("agentos-mode"), mode).unwrap();
    }
    env
}

fn exec(
    env: &Env,
    component: &str,
    counts: &ExecCounts,
    hook: Option<CrashHook>,
) -> RoutingExecutor<FixtureExecutor> {
    let root = env.dir.path();
    let analysis = AnalysisExecutor::new(
        root.join("analysis"),
        root.join("agentos.db"),
        env.snapshot_dir(),
        component_wasm(component),
        counts.clone(),
    )
    .with_crash(hook.clone());
    routing_over(root, env.fixture_exec(), None, counts, hook).with_analysis(analysis)
}

fn fixing_agent() -> FakeAgent {
    FakeAgent::scripted(vec![
        AgentAction::ApplyPatch(fix_patch()),
        AgentAction::Verify,
    ])
}

async fn run(
    env: &Env,
    exec: &RoutingExecutor<FixtureExecutor>,
    agent: &mut FakeAgent,
    opts: &RunOptions,
) -> Result<TaskState, EngineError> {
    run_task_with(&env.db, &env.blobs, exec, agent, &env.task, opts).await
}

/// The task's `AnalyzeSnapshot` effects, in order.
fn analyses(env: &Env) -> Vec<agentos_core::effect::EffectRecord> {
    env.db
        .events(&env.task)
        .unwrap()
        .iter()
        .filter(|e| e.event_type == "EffectIntended" && e.payload["kind"] == "AnalyzeSnapshot")
        .map(|e| {
            env.db
                .effect(&serde_json::from_value(e.payload["effect_id"].clone()).unwrap())
                .unwrap()
        })
        .collect()
}

fn event_types(env: &Env) -> Vec<String> {
    env.db
        .events(&env.task)
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect()
}

#[tokio::test]
async fn the_reference_analyzer_reports_once_before_the_first_turn() {
    let env = env("repo-analyzer-v1", None);
    let counts = ExecCounts::default();
    let exec = exec(&env, "repo-analyzer-v1", &counts, None);
    let state = run(&env, &exec, &mut fixing_agent(), &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded);
    let found = analyses(&env);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].state, EffectState::Completed);
    let report: serde_json::Value =
        serde_json::from_slice(&env.blobs.get(&found[0].result_digest.unwrap()).unwrap()).unwrap();
    assert_eq!(report["analyzer"], "repo-analyzer-v1");
    assert_eq!(report["files"], 4, "{report}");
    assert_eq!(counts.get("analyze_snapshot"), 1);
    let types = event_types(&env);
    let analysis_done = types.iter().position(|t| t == "EffectCompleted").unwrap();
    let snapshot = types.iter().position(|t| t == "WorkspaceUpdated").unwrap();
    let first_turn = types.iter().position(|t| t == "AgentTurn").unwrap();
    assert!(
        snapshot < first_turn && analysis_done < first_turn,
        "{types:?}"
    );
    assert!(types.iter().filter(|t| *t == "EffectCompleted").count() >= 2);
}

#[tokio::test]
async fn a_failing_analyzer_does_not_stop_the_task() {
    let env = env("hostile-analyzer-v1", Some("err"));
    let counts = ExecCounts::default();
    let exec = exec(&env, "hostile-analyzer-v1", &counts, None);
    let state = run(&env, &exec, &mut fixing_agent(), &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded);
    let found = analyses(&env);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].state, EffectState::Failed);
}

/// A report claiming a pass is a report: without a verification the task cannot succeed.
#[tokio::test]
async fn a_report_claiming_a_pass_never_verifies() {
    let env = env("hostile-analyzer-v1", Some("claim-pass"));
    let counts = ExecCounts::default();
    let exec = exec(&env, "hostile-analyzer-v1", &counts, None);
    let mut agent = FakeAgent::scripted(vec![AgentAction::Finish]);
    let state = run(&env, &exec, &mut agent, &RunOptions::default())
        .await
        .unwrap();
    assert_ne!(state, TaskState::Succeeded);
    assert_eq!(analyses(&env)[0].state, EffectState::Completed);
    assert!(!event_types(&env).iter().any(|t| t == "VerifyPassed"));
}

#[tokio::test]
async fn a_revoked_capability_skips_the_analysis_visibly() {
    let env = env("repo-analyzer-v1", None);
    env.db
        .revoke(
            &env.task,
            Some(agentos_core::contract::Capability::SnapshotAnalyze),
        )
        .unwrap();
    let counts = ExecCounts::default();
    let exec = exec(&env, "repo-analyzer-v1", &counts, None);
    let state = run(&env, &exec, &mut fixing_agent(), &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(state, TaskState::Succeeded);
    assert!(analyses(&env).is_empty());
    let skipped = env
        .db
        .events(&env.task)
        .unwrap()
        .into_iter()
        .find(|e| e.event_type == "AnalysisSkipped")
        .expect("the skip is journaled");
    assert!(
        skipped.payload["reason"]
            .as_str()
            .unwrap()
            .contains("snapshot.analyze")
    );
    assert_eq!(counts.get("analyze_snapshot"), 0);
}

/// Every crash point of the analysis: killed once, restarted over the same files, run to the
/// end. One analysis completes; it runs again only when the crash came before its outcome
/// was retained.
#[tokio::test]
async fn every_crash_point_of_the_analysis_recovers_without_running_a_retained_one_again() {
    for (point, runs) in [
        (CrashPoint::AfterIntent, 1),
        (CrashPoint::AfterDispatch, 1),
        (CrashPoint::DuringExecute, 2),
        (CrashPoint::AfterExecuteBeforePublish, 1),
        (CrashPoint::AfterBlobPut, 1),
        (CrashPoint::AfterRegister, 1),
        (CrashPoint::AfterComplete, 1),
    ] {
        let env = env("repo-analyzer-v1", None);
        let counts = ExecCounts::default();
        let hook = CrashHook::at(point, "analyze_snapshot");
        let crashing = exec(&env, "repo-analyzer-v1", &counts, Some(hook.clone()));
        let crashed = run(
            &env,
            &crashing,
            &mut fixing_agent(),
            &RunOptions::crash_with(hook),
        )
        .await;
        assert!(
            matches!(crashed, Err(EngineError::Crashed(p)) if p == point),
            "{point:?}: {crashed:?}"
        );
        drop(crashing);
        let restarted = exec(&env, "repo-analyzer-v1", &counts, None);
        let state = run(
            &env,
            &restarted,
            &mut fixing_agent(),
            &RunOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(state, TaskState::Succeeded, "{point:?}");
        let found = analyses(&env);
        assert_eq!(found.len(), 1, "{point:?}");
        assert_eq!(found[0].state, EffectState::Completed, "{point:?}");
        assert_eq!(
            counts.get("analyze_snapshot"),
            runs,
            "{point:?}: executions"
        );
    }
}
