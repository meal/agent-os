use std::path::Path;

use agentos_core::effect::EffectState;
use agentos_core::ids::TaskId;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_engine::recover::recover;
use serde_json::json;

use super::{print, print_state};
use crate::crash::CrashSpec;
use crate::drive::{agent_patch, drive};
use crate::error::CliError;
use crate::home::Home;

/// Pauses a RUNNING or WAITING task. Works while another process drives it: the runner
/// stops at its next step.
pub fn pause(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    let state = match t.state {
        TaskState::Paused => t.state,
        TaskState::Running | TaskState::Waiting => store.db.append(task, &TaskEvent::Paused).map_err(|e| {
            CliError::other(format!("cannot pause task {task}: {e}"))
        })?.state,
        other => return Err(CliError::other(format!("cannot pause task {task}: it is {}", other.label()))),
    };
    print_state(task, state);
    Ok(())
}

/// What a restarted controller does: approve a READY task, resume a PAUSED one, recover
/// the effects a dead process left in flight, and run the task on.
pub async fn resume(home: &Home, task: &TaskId, patch: Option<&Path>, crash: Option<&CrashSpec>) -> Result<(), CliError> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    if t.state.is_terminal() {
        // A task that ended with effects still in flight (left by an older build or a
        // failure path) gets them decided; otherwise this only reports.
        let in_flight = store.db.outstanding_effects(task)?.iter().any(|e| matches!(e.state, EffectState::Intended | EffectState::Dispatched));
        if in_flight {
            let lock = home.lock()?;
            lock.driving(task)?;
            recover(&store.db, &store.blobs, &home.executor(task)?, task).await?;
        }
        print_state(task, t.state);
        return Ok(());
    }
    let patch = agent_patch(home, task, patch)?;
    let lock = home.lock()?;
    let t = store.db.task(task)?;
    if t.state == TaskState::Paused && !t.cancel_requested {
        // Before recovery, which leaves a paused task's effects untouched.
        store.db.append(task, &TaskEvent::Resumed)?;
    }
    if t.state == TaskState::Ready && !t.cancel_requested {
        // Resuming a READY task is the owner's approval: issue its capability handles.
        store.db.approve_task(task)?;
    }
    let state = if t.state.is_terminal() { t.state } else { drive(home, &store, &lock, task, patch, crash).await? };
    print_state(task, state);
    Ok(())
}

/// Requests cancellation and completes it here after reconciling in-flight effects, unless
/// another process holds the driver lock: then the request stays pending until that process
/// (if it is driving this task) reaches its next step, or the next `resume`/`cancel` of it.
pub async fn cancel(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    if t.state.is_terminal() {
        print(&json!({ "task_id": task, "state": t.state.label(), "note": "already finished; nothing to cancel" }));
        return Ok(());
    }
    if !t.cancel_requested {
        store.db.append(task, &TaskEvent::CancelRequested)?;
    }
    // Running jobs are asked to stop first, whoever drives the task, so the reconciliation
    // below (or the driver's) does not wait out their leases.
    super::revoke::cancel_running_jobs(home, &store, task, None)?;
    let Some(lock) = home.try_lock()? else {
        let state = store.db.task(task)?.state;
        let note = if home.driven_task().as_deref() == Some(task.as_str()) {
            "another agentos process is driving this task; it completes the cancel at its next step".to_string()
        } else {
            format!(
                "another agentos process is driving tasks; the cancel completes on the next `agentos resume {task}` or `agentos cancel {task}`"
            )
        };
        print(&json!({ "task_id": task, "state": state.label(), "cancel_requested": true, "note": note }));
        return Ok(());
    };
    lock.driving(task)?;
    let t = store.db.task(task)?;
    if !t.state.is_terminal() {
        if store.db.outstanding_effects(task)?.is_empty() {
            store.db.append(task, &TaskEvent::CancelCompleted)?;
        } else {
            // Reconciles in-flight effects and completes the cancel once none is in flight.
            recover(&store.db, &store.blobs, &home.executor(task)?, task).await?;
        }
    }
    print_state(task, store.db.task(task)?.state);
    Ok(())
}
