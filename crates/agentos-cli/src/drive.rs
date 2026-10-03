//! Driving a task as a (re)started controller: recover what a dead process left in flight,
//! then run the agent loop until the task finishes, pauses, or the crash flag kills us.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use agentos_core::ids::TaskId;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::{Agent, AgentAction, FakeAgent, ModelAgent, Observation};
use agentos_engine::crash::{CrashPoint, RunOptions};
use agentos_engine::recover::recover_with;
use agentos_engine::runner::{run_task_with, EngineError};
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::supervised::SupervisedExecutor;
use agentos_engine::workspace::workspace_digest;
use serde_json::json;

use crate::crash::{point_name, CrashSpec};
use crate::error::CliError;
use crate::home::{DriverLock, Home, Store};

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
        let unknown = || format!("unknown model spec {s:?}; expected anthropic:<model> or fake:<transcript file>");
        if let Some(model) = s.strip_prefix("anthropic:").filter(|m| !m.is_empty()) {
            // A model name reaches the journal and the request body: plain, bounded.
            let plain = model.len() <= SPEC_NAME_LIMIT && model.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            return if plain {
                Ok(ModelSpec::Anthropic(model.to_string()))
            } else {
                Err(format!("invalid model name {model:?}: at most {SPEC_NAME_LIMIT} characters from A-Z a-z 0-9 . _ -"))
            };
        }
        match s.strip_prefix("fake:").filter(|p| !p.is_empty()) {
            Some(path) => {
                let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if name.is_empty() || name.len() > SPEC_NAME_LIMIT || name.chars().any(char::is_control) {
                    return Err(format!("invalid transcript file name {name:?}: 1 to {SPEC_NAME_LIMIT} bytes, no control characters"));
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
            ModelSpec::Fake(path) => format!("fake:{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        }
    }
}

/// The agent a task runs: the fake agent over a patch, or the model agent.
pub enum Driver {
    Fake(FakeAgent),
    Model(Box<ModelAgent>),
}

impl Agent for Driver {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        match self {
            Driver::Fake(a) => a.next(obs),
            Driver::Model(a) => a.next(obs),
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
        Err(e) => Err(CliError::usage(format!("cannot read {}: {e}", path.display()))),
    }
}

/// The agent for `task` from what `Submitted` recorded: the model agent for a model task (it
/// needs no patch and refuses `patch_flag`), the fake agent over `patch_flag` or the recorded
/// patch otherwise.
pub fn agent_for(home: &Home, store: &Store, task: &TaskId, patch_flag: Option<&Path>) -> Result<Driver, CliError> {
    let recorded = home.recorded_model(store, task)?;
    let model = match recorded.as_deref() {
        None | Some(FAKE_AGENT) => None,
        Some(m) => Some(match m.strip_prefix("anthropic:") {
            // The recorded name is re-validated: the journal is not trusted blindly.
            Some(name) => match m.parse::<ModelSpec>() {
                Ok(ModelSpec::Anthropic(_)) => name.to_string(),
                _ => return Err(CliError::other(format!("task {task} records an invalid model {m:?}"))),
            },
            None if m.starts_with("fake:") => "fake".to_string(),
            None => return Err(CliError::other(format!("task {task} records an unknown model {m:?}"))),
        }),
    };
    match model {
        None => Ok(Driver::Fake(FakeAgent::from_fixture_patch(agent_patch(home, task, patch_flag)?))),
        Some(_) if patch_flag.is_some() => Err(CliError::usage(format!("task {task} runs a model, not the fake agent"))),
        Some(name) => Ok(Driver::Model(Box::new(ModelAgent::new(store.db.contract(task)?, name)))),
    }
}

/// A real process death: no cleanup, no further output.
fn crash_exit(task: &TaskId, point: CrashPoint) -> ! {
    eprintln!("{}", json!({ "crashed": point_name(point), "task_id": task }));
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
        .ok_or_else(|| CliError::other(format!("task {task} was not completely submitted; cancel it and submit again")))?;
    let dir = home.task_dir(task);
    for (sub, field) in [("snapshot", "repository_digest"), ("profile", "profile_digest")] {
        let recorded = submitted.payload[field].as_str().unwrap_or_default();
        let actual = workspace_digest(&dir.join(sub)).map_err(|e| CliError::other(format!("cannot digest recorded {sub}: {e}")))?;
        if actual.to_string() != recorded {
            let reason = format!("recorded {sub} changed: expected {recorded}, found {actual}");
            return Ok(Some(store.db.append(task, &TaskEvent::Failed { reason })?.state));
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
    let RoutingExecutor { jobs, model, reads } = exec;
    let exec = RoutingExecutor::new(jobs.with_crash(hook.clone()), model.with_crash(hook.clone()), reads.with_crash(hook.clone()));
    let opts = RunOptions { crash: hook };
    if let Some(state) = check_inputs(home, store, task)? {
        // The task is failed before anything runs on the changed inputs, but what the dead
        // process left in flight must still be decided: on a terminal task recovery
        // dispatches nothing, it only publishes retained receipts, reconciles, and marks the
        // rest unknown or abandoned, so no reservation is stranded as Reserved.
        survive(task, recover_with(&store.db, &store.blobs, &exec, task, &opts).await)?;
        return Ok(state);
    }
    tracing::info!(task_id = %task, "driving task");
    survive(task, recover_with(&store.db, &store.blobs, &exec, task, &opts).await)?;
    survive(task, run_task_with(&store.db, &store.blobs, &exec, &mut agent, task, &opts).await)
}
