//! `revoke`: withdraw a task's capability handles and stop the running jobs that depend on
//! them. It never takes the driver lock: a revocation must reach a task another process is
//! driving, which meets it at its next authorization.

use agentos_core::contract::Capability;
use agentos_core::ids::TaskId;
use agentos_engine::supervised::cancel_jobs_in;
use serde_json::{Value, json};

use super::print;
use crate::error::CliError;
use crate::home::{Home, Store};

fn capability_name(cap: Capability) -> String {
    serde_json::to_value(cap)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Every capability name a contract may list (the serde names of [`Capability`]).
const CAPABILITY_NAMES: [&str; 7] = [
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "artifact.export",
    "model.request",
    "snapshot.analyze",
    "agent.session",
];

fn parse_capability(name: &str) -> Result<Capability, CliError> {
    serde_json::from_value(Value::String(name.to_string())).map_err(|_| {
        CliError::usage(format!(
            "unknown capability {name:?}; expected one of {}",
            CAPABILITY_NAMES.join(", ")
        ))
    })
}

/// Drops the `cancel` marker of every live job of the task's outstanding effects (only
/// those whose kind needs a capability in `only`, when given); their supervisors kill the
/// workers. Returns how many jobs were asked to stop.
pub fn cancel_running_jobs(
    home: &Home,
    store: &Store,
    task: &TaskId,
    only: Option<&[Capability]>,
) -> Result<usize, CliError> {
    let effects: Vec<_> = store
        .db
        .outstanding_effects(task)?
        .into_iter()
        .filter(|e| only.is_none_or(|caps| caps.contains(&e.kind.capability())))
        .map(|e| e.effect_id)
        .collect();
    // Dropping a cancel marker needs no worker (and so no preflight): whatever the worker,
    // the job's supervisor kills it within a poll interval.
    Ok(cancel_jobs_in(
        &std::path::absolute(&home.root)?.join("jobs"),
        &effects,
    ))
}

pub fn revoke(home: &Home, task: &TaskId, capability: Option<&str>) -> Result<(), CliError> {
    let only = capability.map(parse_capability).transpose()?;
    let store = home.open()?;
    let revoked = store.db.revoke(task, only)?;
    let finished = store.db.task(task)?.state.is_terminal();
    let cancelled = if finished || revoked.is_empty() {
        0
    } else {
        cancel_running_jobs(home, &store, task, Some(&revoked))?
    };
    print(&json!({
        "task_id": task,
        "revoked": revoked.into_iter().map(capability_name).collect::<Vec<_>>(),
        "cancelled_jobs": cancelled,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names listed in the error all parse, and every one of them is listed.
    #[test]
    fn the_listed_capability_names_all_parse() {
        for name in CAPABILITY_NAMES {
            assert!(parse_capability(name).is_ok(), "{name}");
        }
        assert!(parse_capability("teleport.now").is_err());
    }
}
