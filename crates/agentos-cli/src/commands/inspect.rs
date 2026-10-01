use agentos_core::ids::TaskId;
use serde_json::json;

use super::print;
use crate::error::CliError;
use crate::home::Home;

pub fn status(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    let outstanding: Vec<_> = store
        .db
        .outstanding_effects(task)?
        .into_iter()
        .map(|e| json!({ "effect_id": e.effect_id, "kind": e.kind.tag(), "state": e.state, "lease_generation": e.lease_generation }))
        .collect();
    print(&json!({
        "task_id": t.id,
        "state": t.state.label(),
        "step": t.step,
        "cancel_requested": t.cancel_requested,
        "workspace_digest": t.workspace_digest,
        "verified_digest": t.verified_digest,
        "actions_used": t.actions_used,
        "usage": store.db.usage_summary(task)?,
        "outstanding_effects": outstanding,
    }));
    Ok(())
}

pub fn events(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    for e in store.db.events(task)? {
        print(&json!({ "seq": e.seq, "type": e.event_type, "payload": e.payload, "ts": e.ts }));
    }
    Ok(())
}
