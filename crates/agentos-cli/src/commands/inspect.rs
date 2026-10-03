use agentos_core::ids::TaskId;
use serde_json::json;

use agentos_core::effect::EffectId;
use agentos_engine::job::JobDir;
use serde_json::Value;

use super::print;
use crate::drive::FAKE_AGENT;
use crate::error::CliError;
use crate::home::Home;

pub fn status(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    let effects = store.db.outstanding_effects(task)?;
    let jobs_root = std::path::absolute(&home.root)?.join("jobs");
    let jobs: Vec<_> = effects.iter().filter_map(|e| latest_job(&jobs_root, &e.effect_id)).collect();
    let outstanding: Vec<_> = effects
        .into_iter()
        .map(|e| json!({ "effect_id": e.effect_id, "kind": e.kind.tag(), "state": e.state, "lease_generation": e.lease_generation }))
        .collect();
    // Prefixes only: a full handle is never printed.
    let capabilities: Vec<_> = store
        .db
        .grants(task)?
        .into_iter()
        .map(|g| json!({ "operation": g.operation, "handle_prefix": g.handle.prefix(), "revoked": g.revoked, "expires_ts": g.expires_ts }))
        .collect();
    let worker = home.recorded_worker(&store, task)?;
    let mut shown = json!({
        "task_id": t.id,
        "state": t.state.label(),
        "step": t.step,
        "cancel_requested": t.cancel_requested,
        "workspace_digest": t.workspace_digest,
        "verified_digest": t.verified_digest,
        "actions_used": t.actions_used,
        "usage": store.db.usage_summary(task)?,
        "outstanding_effects": outstanding,
        "jobs": jobs,
        "capabilities": capabilities,
        "worker": worker.kind.as_str(),
        "model": home.recorded_model(&store, task)?.unwrap_or_else(|| FAKE_AGENT.to_string()),
    });
    if let Some((id, digest)) = &worker.image {
        shown["guest_image"] = json!({ "id": id, "digest": digest });
    }
    if let Some(jailed) = worker.jailed {
        shown["jailed"] = json!(jailed);
    }
    print(&shown);
    Ok(())
}

pub fn events(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let store = home.open()?;
    for e in store.db.events(task)? {
        print(&json!({ "seq": e.seq, "type": e.event_type, "payload": e.payload, "ts": e.ts }));
    }
    Ok(())
}

/// The job of the highest lease generation of `effect`: where it stands, whether its
/// supervisor still holds the lock (`alive`), and whether a receipt is on disk.
fn latest_job(jobs_root: &std::path::Path, effect: &EffectId) -> Option<Value> {
    let job = JobDir::list(jobs_root, effect).ok()?.into_iter().next_back()?;
    let request = job.request().ok()?;
    let state = job.read_status().map(|s| serde_json::to_value(s.state).unwrap_or(Value::Null)).unwrap_or(Value::Null);
    Some(json!({
        "effect_id": effect,
        "attempt_id": request.attempt_id,
        "lease_generation": request.lease_generation,
        "state": state,
        "alive": !job.is_dead(),
        "receipt": job.read_receipt().is_some(),
    }))
}
