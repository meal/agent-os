//! Driving a task as a (re)started controller: recover what a dead process left in flight,
//! then run the agent loop until the task finishes, pauses, or the crash flag kills us.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use agentos_core::ids::TaskId;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{
    Agent, AgentAction, FakeAgent, ModelAgent, Observation, SessionAgent, claude_code_argv,
};
use agentos_engine::crash::{CrashPoint, RunOptions};
use agentos_engine::recover::recover_with;
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::runner::{EngineError, run_task_with};
use agentos_engine::supervised::SupervisedExecutor;
use agentos_engine::workspace::workspace_digest;
use serde_json::json;

use crate::crash::{CrashSpec, point_name};
use crate::error::CliError;
use crate::home::{DriverLock, Home, Store, agent_argv_hook};

/// Exit code of a process killed by `--crash-at`.
pub const CRASH_EXIT: i32 = 75;

pub const AGENT_PATCH: &str = "agent.patch";
/// The task's own copy of a `fake:` transcript.
pub const TRANSCRIPT: &str = "transcript.json";
/// `Submitted.model` of a task run by the fake agent.
pub const FAKE_AGENT: &str = "fake-agent";

/// The longest model name or transcript file name recorded: both land in the journal, in
/// request bodies and in listings.
const SPEC_NAME_LIMIT: usize = 100;

/// What `--model` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSpec {
    /// A real model of the Anthropic Messages API.
    Anthropic(String),
    /// The scripted provider over this transcript file.
    Fake(PathBuf),
}

impl FromStr for ModelSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<ModelSpec, String> {
        let unknown = || {
            format!(
                "unknown model spec {s:?}; expected anthropic:<model> or fake:<transcript file>"
            )
        };
        if let Some(model) = s.strip_prefix("anthropic:").filter(|m| !m.is_empty()) {
            // A model name reaches the journal and the request body: plain, bounded.
            let plain = model.len() <= SPEC_NAME_LIMIT
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            return if plain {
                Ok(ModelSpec::Anthropic(model.to_string()))
            } else {
                Err(format!(
                    "invalid model name {model:?}: at most {SPEC_NAME_LIMIT} characters from A-Z a-z 0-9 . _ -"
                ))
            };
        }
        match s.strip_prefix("fake:").filter(|p| !p.is_empty()) {
            Some(path) => {
                let name = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.is_empty()
                    || name.len() > SPEC_NAME_LIMIT
                    || name.chars().any(char::is_control)
                {
                    return Err(format!(
                        "invalid transcript file name {name:?}: 1 to {SPEC_NAME_LIMIT} bytes, no control characters"
                    ));
                }
                Ok(ModelSpec::Fake(PathBuf::from(path)))
            }
            None => Err(unknown()),
        }
    }
}

impl ModelSpec {
    /// What `Submitted.model` records: `anthropic:<model>` or `fake:<file name>`.
    pub fn recorded(&self) -> String {
        match self {
            ModelSpec::Anthropic(model) => format!("anthropic:{model}"),
            ModelSpec::Fake(path) => format!(
                "fake:{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
        }
    }
}

/// The coding-agent CLIs `--agent-cli` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// Claude Code, from the guest image's `/opt/agent-cli/claude`.
    ClaudeCode,
}

impl AgentCli {
    /// The name `--agent-cli` takes and `Submitted.agent_cli` records.
    pub fn name(self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "claude-code",
        }
    }
}

impl FromStr for AgentCli {
    type Err = String;

    fn from_str(s: &str) -> Result<AgentCli, String> {
        match s {
            "claude-code" => Ok(AgentCli::ClaudeCode),
            _ => Err(format!("unknown agent CLI {s:?}; known: claude-code")),
        }
    }
}

/// The agent a task runs: the fake agent over a patch, the model agent, or a coding-agent CLI
/// session.
pub enum Driver {
    Fake(FakeAgent),
    Model(Box<ModelAgent>),
    Session(Box<SessionAgent>),
}

impl Agent for Driver {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        match self {
            Driver::Fake(a) => a.next(obs),
            Driver::Model(a) => a.next(obs),
            Driver::Session(a) => a.next(obs),
        }
    }
}

/// The fake agent's patch: `flag` if given, else the one recorded at submission.
pub fn agent_patch(home: &Home, task: &TaskId, flag: Option<&Path>) -> Result<String, CliError> {
    let path = match flag {
        Some(p) => p.to_path_buf(),
        None => home.task_dir(task).join(AGENT_PATCH),
    };
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(_) if flag.is_none() => Err(CliError::usage(format!(
            "task {task} has no agent: pass --fake-agent-patch FILE (or submit it with --model anthropic:<model>|fake:<transcript>)"
        ))),
        Err(e) => Err(CliError::usage(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// The agent for `task` from what `Submitted` recorded: the model agent for a model task (it
/// needs no patch and refuses `patch_flag`), the fake agent over `patch_flag` or the recorded
/// patch otherwise.
pub fn agent_for(
    home: &Home,
    store: &Store,
    task: &TaskId,
    patch_flag: Option<&Path>,
) -> Result<Driver, CliError> {
    if let Some(cli) = home.recorded_agent_cli(store, task)? {
        return session_for(home, store, task, &cli, patch_flag);
    }
    let recorded = home.recorded_model(store, task)?;
    let model = match recorded.as_deref() {
        None | Some(FAKE_AGENT) => None,
        Some(m) => Some(match m.strip_prefix("anthropic:") {
            // The recorded name is re-validated: the journal is not trusted blindly.
            Some(name) => match m.parse::<ModelSpec>() {
                Ok(ModelSpec::Anthropic(_)) => name.to_string(),
                _ => {
                    return Err(CliError::other(format!(
                        "task {task} records an invalid model {m:?}"
                    )));
                }
            },
            None if m.starts_with("fake:") => "fake".to_string(),
            None => {
                return Err(CliError::other(format!(
                    "task {task} records an unknown model {m:?}"
                )));
            }
        }),
    };
    match model {
        None => Ok(Driver::Fake(FakeAgent::from_fixture_patch(agent_patch(
            home, task, patch_flag,
        )?))),
        Some(_) if patch_flag.is_some() => Err(CliError::usage(format!(
            "task {task} runs a model, not the fake agent"
        ))),
        Some(name) => Ok(Driver::Model(Box::new(ModelAgent::new(
            store.db.contract(task)?,
            name,
        )))),
    }
}

/// The session agent of a task whose `Submitted` recorded an agent CLI. The CLI runs with the
/// preset argv for the contract's goal (or the test hook's), an empty env, and the model the
/// task recorded; the recorded CLI and model are re-validated, the journal not being trusted.
fn session_for(
    home: &Home,
    store: &Store,
    task: &TaskId,
    recorded: &str,
    patch_flag: Option<&Path>,
) -> Result<Driver, CliError> {
    let cli: AgentCli = recorded.parse().map_err(|_| {
        CliError::other(format!(
            "task {task} records an unknown agent CLI {recorded:?}"
        ))
    })?;
    let model = match home.recorded_model(store, task)?.as_deref() {
        None | Some(FAKE_AGENT) => {
            return Err(CliError::other(format!(
                "task {task} records agent CLI {} with no model",
                cli.name()
            )));
        }
        Some(m) => match m.strip_prefix("anthropic:") {
            Some(name) => match m.parse::<ModelSpec>() {
                Ok(ModelSpec::Anthropic(_)) => name.to_string(),
                _ => {
                    return Err(CliError::other(format!(
                        "task {task} records an invalid model {m:?}"
                    )));
                }
            },
            None if m.starts_with("fake:") => "fake".to_string(),
            None => {
                return Err(CliError::other(format!(
                    "task {task} records an unknown model {m:?}"
                )));
            }
        },
    };
    if patch_flag.is_some() {
        return Err(CliError::usage(format!(
            "task {task} runs an agent CLI session, not the fake agent"
        )));
    }
    let goal = store.db.contract(task)?.goal;
    let preset = match cli {
        AgentCli::ClaudeCode => claude_code_argv(&goal),
    }
    .map_err(|e| CliError::usage(format!("task {task}: {e}")))?;
    let argv = agent_argv_hook(|k| std::env::var(k).ok())?.unwrap_or(preset);
    Ok(Driver::Session(Box::new(SessionAgent::new(
        argv,
        Vec::new(),
        model,
    ))))
}

/// A real process death: no cleanup, no further output.
fn crash_exit(task: &TaskId, point: CrashPoint) -> ! {
    eprintln!(
        "{}",
        json!({ "crashed": point_name(point), "task_id": task })
    );
    std::process::exit(CRASH_EXIT)
}

fn survive<T>(task: &TaskId, r: Result<T, EngineError>) -> Result<T, CliError> {
    match r {
        Err(EngineError::Crashed(point)) => crash_exit(task, point),
        r => r.map_err(CliError::from),
    }
}

/// The inputs recorded at submission must still be what was approved; a task whose inputs
/// changed is failed rather than run on something else.
fn check_inputs(home: &Home, store: &Store, task: &TaskId) -> Result<Option<TaskState>, CliError> {
    let events = store.db.events(task)?;
    let submitted = events
        .iter()
        .find(|e| e.event_type == "Submitted")
        .ok_or_else(|| {
            CliError::other(format!(
                "task {task} was not completely submitted; cancel it and submit again"
            ))
        })?;
    let dir = home.task_dir(task);
    for (sub, field) in [
        ("snapshot", "repository_digest"),
        ("profile", "profile_digest"),
    ] {
        let recorded = submitted.payload[field].as_str().unwrap_or_default();
        let actual = workspace_digest(&dir.join(sub))
            .map_err(|e| CliError::other(format!("cannot digest recorded {sub}: {e}")))?;
        if actual.to_string() != recorded {
            let reason = format!("recorded {sub} changed: expected {recorded}, found {actual}");
            return Ok(Some(
                store.db.append(task, &TaskEvent::Failed { reason })?.state,
            ));
        }
    }
    Ok(None)
}

/// Recovers `task` and runs it with `agent` on `exec` (built by the caller before it changed
/// anything, so a worker that cannot run leaves the task untouched). The caller holds the
/// driver lock.
pub async fn drive(
    home: &Home,
    store: &Store,
    lock: &DriverLock,
    task: &TaskId,
    mut agent: Driver,
    crash: Option<&CrashSpec>,
    exec: RoutingExecutor<SupervisedExecutor>,
) -> Result<TaskState, CliError> {
    lock.driving(task)?;
    let hook = crash.map(CrashSpec::hook);
    // The crash hook reaches every executor the router owns.
    let RoutingExecutor {
        jobs,
        model,
        reads,
        analysis,
    } = exec;
    let mut exec = RoutingExecutor::new(
        jobs.with_crash(hook.clone()),
        model.with_crash(hook.clone()),
        reads.with_crash(hook.clone()),
    );
    if let Some(analysis) = analysis {
        exec = exec.with_analysis(analysis.with_crash(hook.clone()));
    }
    let opts = RunOptions { crash: hook };
    if let Some(state) = check_inputs(home, store, task)? {
        // The task is failed before anything runs on the changed inputs, but what the dead
        // process left in flight must still be decided: on a terminal task recovery
        // dispatches nothing, it only publishes retained receipts, reconciles, and marks the
        // rest unknown or abandoned, so no reservation is stranded as Reserved.
        survive(
            task,
            recover_with(&store.db, &store.blobs, &exec, task, &opts).await,
        )?;
        return Ok(state);
    }
    tracing::info!(task_id = %task, "driving task");
    survive(
        task,
        recover_with(&store.db, &store.blobs, &exec, task, &opts).await,
    )?;
    survive(
        task,
        run_task_with(&store.db, &store.blobs, &exec, &mut agent, task, &opts).await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::ids::Digest;
    use agentos_engine::agent::{CLAUDE_CODE_PATH, claude_code_argv};
    use serde_json::{Value, json};

    const GOAL: &str = "fix the parser";

    /// A home with one task whose contract has `goal` and whose `Submitted` event is `payload`.
    fn submitted(dir: &Path, goal: &str, payload: Value) -> (Home, Store, TaskId) {
        let home = Home::new(Some(dir.join("home")), None).unwrap();
        let store = home.open().unwrap();
        let contract = Contract::parse(
            &json!({
                "goal": goal,
                "repository": { "source": dir, "revision": "recorded-at-submission" },
                "profile": "python-stdlib-v1",
                "editable_paths": ["src/**"],
                "verification_profile": "parser-checks-v1",
                "capabilities": ["snapshot.read", "model.request", "agent.session"],
                "limits": {
                    "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 3,
                    "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256
                }
            })
            .to_string(),
        )
        .unwrap();
        let task = store
            .db
            .create_task(&contract, &Digest::of(b"contract"))
            .unwrap();
        store.db.append_audit(&task, "Submitted", &payload).unwrap();
        (home, store, task)
    }

    fn start() -> Observation {
        Observation::Start {
            files: vec![],
            workspace: Digest::of(b"ws"),
        }
    }

    fn session_payload(model: &str) -> Value {
        json!({ "agent_cli": "claude-code", "model": model })
    }

    #[test]
    fn a_recorded_claude_code_session_runs_the_preset_with_an_empty_env_and_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            GOAL,
            session_payload("anthropic:claude-opus-5-5"),
        );
        let mut driver = agent_for(&home, &store, &task, None).unwrap();
        assert!(matches!(driver, Driver::Session(_)));
        assert_eq!(
            driver.next(&start()),
            AgentAction::RunSession {
                argv: claude_code_argv(GOAL).unwrap(),
                env: vec![],
                model: "claude-opus-5-5".into(),
            }
        );
        assert_eq!(claude_code_argv(GOAL).unwrap()[0], CLAUDE_CODE_PATH);
    }

    #[test]
    fn a_fake_transcript_session_is_served_by_the_fake_provider_model() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) =
            submitted(dir.path(), GOAL, session_payload("fake:parser-fix.json"));
        let mut driver = agent_for(&home, &store, &task, None).unwrap();
        match driver.next(&start()) {
            AgentAction::RunSession { model, .. } => assert_eq!(model, "fake"),
            other => panic!("expected RunSession, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_recorded_agent_cli_is_refused_as_a_recording_error() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            GOAL,
            json!({ "agent_cli": "codex", "model": "anthropic:claude-opus-5-5" }),
        );
        let err = agent_for(&home, &store, &task, None).err().unwrap();
        assert_eq!(err.code, 1);
        assert!(err.message.contains("unknown agent CLI"), "{}", err.message);
    }

    #[test]
    fn a_recorded_agent_cli_without_a_real_model_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for payload in [
            json!({ "agent_cli": "claude-code" }),
            json!({ "agent_cli": "claude-code", "model": "fake-agent" }),
        ] {
            let (home, store, task) = submitted(dir.path(), GOAL, payload.clone());
            let err = agent_for(&home, &store, &task, None).err().unwrap();
            assert_eq!(err.code, 1, "{payload}");
            assert!(
                err.message.contains("no model"),
                "{payload}: {}",
                err.message
            );
        }
    }

    #[test]
    fn a_patch_flag_for_a_session_task_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            GOAL,
            session_payload("anthropic:claude-opus-5-5"),
        );
        let err = agent_for(&home, &store, &task, Some(Path::new("missing.diff")))
            .err()
            .unwrap();
        assert_eq!(err.code, 2);
        assert_eq!(
            err.message,
            format!("task {task} runs an agent CLI session, not the fake agent")
        );
    }

    #[test]
    fn a_goal_the_preset_refuses_is_a_usage_error_when_the_session_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            "-p",
            session_payload("anthropic:claude-opus-5-5"),
        );
        let err = agent_for(&home, &store, &task, None).err().unwrap();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("'-'"), "{}", err.message);
    }

    #[test]
    fn a_task_without_agent_cli_still_gets_the_model_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            GOAL,
            json!({ "model": "anthropic:claude-opus-5-5" }),
        );
        assert!(matches!(
            agent_for(&home, &store, &task, None).unwrap(),
            Driver::Model(_)
        ));
    }

    #[test]
    fn recorded_agent_cli_reads_the_submitted_event_and_is_none_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task) = submitted(
            dir.path(),
            GOAL,
            session_payload("anthropic:claude-opus-5-5"),
        );
        assert_eq!(
            home.recorded_agent_cli(&store, &task).unwrap(),
            Some("claude-code".into())
        );
        let other_dir = tempfile::tempdir().unwrap();
        let (home, store, task) =
            submitted(other_dir.path(), GOAL, json!({ "model": "fake-agent" }));
        assert_eq!(home.recorded_agent_cli(&store, &task).unwrap(), None);
    }

    #[test]
    fn the_agent_cli_names_are_exactly_the_known_presets() {
        assert_eq!("claude-code".parse::<AgentCli>(), Ok(AgentCli::ClaudeCode));
        assert_eq!(AgentCli::ClaudeCode.name(), "claude-code");
        for bad in ["", "Claude-Code", "claude", "claude-code ", "codex"] {
            let err = bad.parse::<AgentCli>().unwrap_err();
            assert!(err.contains("claude-code"), "{bad:?}: {err}");
        }
    }
}
