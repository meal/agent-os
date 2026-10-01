//! Driving a task as a (re)started controller: recover what a dead process left in flight,
//! then run the agent loop until the task finishes, pauses, or the crash flag kills us.

use std::fs;
use std::path::Path;

use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::agent::FakeAgent;
use agentos_engine::crash::{CrashPoint, RunOptions};
use agentos_engine::recover::recover_with;
use agentos_engine::runner::{run_task_with, EngineError};
use agentos_engine::workspace::workspace_digest;
use serde_json::json;

use crate::crash::{point_name, CrashSpec};
use crate::error::CliError;
use crate::home::{DriverLock, Home, Store};

/// Exit code of a process killed by `--crash-at`.
pub const CRASH_EXIT: i32 = 75;

pub const AGENT_PATCH: &str = "agent.patch";

/// The fake agent's patch: `flag` if given, else the one recorded at submission.
pub fn agent_patch(home: &Home, task: &TaskId, flag: Option<&Path>) -> Result<String, CliError> {
    let path = match flag {
        Some(p) => p.to_path_buf(),
        None => home.task_dir(task).join(AGENT_PATCH),
    };
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(_) if flag.is_none() => Err(CliError::usage(format!(
            "task {task} has no agent: pass --fake-agent-patch FILE (the fake agent is the only agent until the model broker exists)"
        ))),
        Err(e) => Err(CliError::usage(format!("cannot read {}: {e}", path.display()))),
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

/// Recovers `task` and runs it with a fresh fake agent. The caller holds the driver lock.
pub async fn drive(home: &Home, store: &Store, lock: &DriverLock, task: &TaskId, patch: String, crash: Option<&CrashSpec>) -> Result<TaskState, CliError> {
    lock.driving(task)?;
    let hook = crash.map(CrashSpec::hook);
    let exec = home.executor(task)?.with_crash(hook.clone());
    let opts = RunOptions { crash: hook };
    if let Some(state) = check_inputs(home, store, task)? {
        // The task is failed before anything runs on the changed inputs, but what the dead
        // process left in flight must still be decided: on a terminal task recovery
        // dispatches nothing, it only publishes retained receipts, reconciles, and marks the
        // rest unknown or abandoned, so no reservation is stranded as Reserved.
        survive(task, recover_with(&store.db, &store.blobs, &exec, task, &opts).await)?;
        return Ok(state);
    }
    tracing::info!(task_id = %task, agent_patch = %Digest::of(patch.as_bytes()), "driving task");
    survive(task, recover_with(&store.db, &store.blobs, &exec, task, &opts).await)?;
    let mut agent = FakeAgent::from_fixture_patch(patch);
    survive(task, run_task_with(&store.db, &store.blobs, &exec, &mut agent, task, &opts).await)
}
