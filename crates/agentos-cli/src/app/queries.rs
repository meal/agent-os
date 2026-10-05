use super::types::*;
use super::{AppError, AppErrorKind, AppResult};
use crate::home::Home;
use agentos_core::ids::TaskId;
use agentos_core::state::TaskState;
use agentos_engine::export::ReviewContents;
use agentos_store::read::{EventPage, TaskCursor};

pub(crate) const CONTRACT_LIMIT: u64 = 256 * 1024;
pub(crate) const RESULT_LIMIT: u64 = 128 * 1024 * 1024;
use crate::home::provenance;
use agentos_core::broker::Resource;
use agentos_core::contract::Capability;
use agentos_core::effect::EffectId;
use agentos_core::ids::Digest;
use agentos_engine::export::collect_review;
use agentos_engine::job::JobDir;
use agentos_engine::model::policy::versions_from_submitted;
use agentos_store::db::Db;

fn submitted(
    db: &Db,
    task: &TaskId,
    max_bytes: Option<u64>,
) -> AppResult<Option<serde_json::Value>> {
    Ok(db
        .first_event_bounded(task, "Submitted", max_bytes)?
        .map(|e| e.payload))
}
pub(crate) fn status(home: &Home, task: &TaskId, max_bytes: Option<u64>) -> AppResult<StatusView> {
    let store = home.open()?;
    if max_bytes.is_some() {
        store.db.contract_bounded(task, CONTRACT_LIMIT)?;
    }
    let t = store.db.task(task)?;
    let effects = match max_bytes {
        Some(limit) => store.db.outstanding_effects_bounded(task, limit)?,
        None => store.db.outstanding_effects(task)?,
    };
    let jobs_root = std::path::absolute(&home.root)?.join("jobs");
    let jobs = effects
        .iter()
        .filter_map(|e| latest_job(&jobs_root, &e.effect_id))
        .collect();
    let outstanding_effects = effects
        .into_iter()
        .map(|e| OutstandingEffect {
            effect_id: e.effect_id,
            kind: e.kind.tag().into(),
            state: e.state,
            lease_generation: e.lease_generation,
        })
        .collect();
    let capabilities = store
        .db
        .grants(task)?
        .into_iter()
        .map(|g| CapabilitySummary {
            operation: g.operation,
            handle_prefix: g.handle.prefix().into(),
            revoked: g.revoked,
            expires_ts: g.expires_ts,
        })
        .collect();
    let p = submitted(&store.db, task, max_bytes)?;
    let worker = provenance::worker(task, p.as_ref())?;
    let (model_policy_version, model_limits_version) = versions_from_submitted(p.as_ref())?;
    Ok(StatusView {
        task_id: t.id,
        state: t.state.label().into(),
        step: t.step,
        cancel_requested: t.cancel_requested,
        workspace_digest: t.workspace_digest,
        verified_digest: t.verified_digest,
        actions_used: t.actions_used,
        usage: store.db.usage_summary(task)?,
        outstanding_effects,
        jobs,
        capabilities,
        worker: worker.kind.as_str().into(),
        model: provenance::model(p.as_ref()).unwrap_or_else(|| crate::drive::FAKE_AGENT.into()),
        model_policy_version,
        model_limits_version,
        model_endpoint: provenance::endpoint(p.as_ref())?,
        guest_image: worker
            .image
            .map(|(id, digest)| GuestImageRef { id, digest }),
        jailed: worker.jailed,
    })
}
pub(crate) fn tasks(
    home: &Home,
    filter: Option<TaskState>,
    cursor: Option<&TaskCursor>,
) -> AppResult<TaskListView> {
    let store = home.open()?;
    let page = store.db.tasks_page(filter, cursor, 50)?;
    let mut rows = Vec::new();
    for row in page.rows {
        let p = submitted(&store.db, &row.task.id, Some(64 * 1024))?;
        let worker = provenance::worker(&row.task.id, p.as_ref())?
            .kind
            .as_str()
            .into();
        let model =
            provenance::model(p.as_ref()).unwrap_or_else(|| crate::drive::FAKE_AGENT.into());
        rows.push(TaskSummary { row, worker, model });
    }
    Ok(TaskListView {
        rows,
        next: page.next,
    })
}
pub(crate) fn detail(home: &Home, task: &TaskId) -> AppResult<TaskDetail> {
    let store = home.open()?;
    let contract = store.db.contract_bounded(task, CONTRACT_LIMIT)?;
    let created = store
        .db
        .first_event_bounded(task, "TaskCreated", Some(64 * 1024))?
        .ok_or_else(|| AppError::new(AppErrorKind::Unavailable, "TaskCreated event is missing"))?;
    let contract_digest = required_digest(&created.payload, "contract_digest")?;
    let p = submitted(&store.db, task, Some(64 * 1024))?;
    let optional = |field| {
        p.as_ref()
            .filter(|p| !p[field].is_null())
            .map(|p| required_digest(p, field))
            .transpose()
    };
    let repository_digest = optional("repository_digest")?;
    let profile_digest = optional("profile_digest")?;
    Ok(TaskDetail {
        status: status(home, task, Some(RESULT_LIMIT))?,
        contract,
        contract_digest,
        repository_digest,
        profile_digest,
    })
}
fn required_digest(p: &serde_json::Value, field: &str) -> AppResult<Digest> {
    p[field]
        .as_str()
        .and_then(|s| Digest::from_hex(s).ok())
        .ok_or_else(|| {
            AppError::new(
                AppErrorKind::Unavailable,
                format!("missing or invalid recorded {field}"),
            )
        })
}
pub(crate) fn events(home: &Home, task: &TaskId, after: u64) -> AppResult<EventPage> {
    Ok(home.open()?.db.event_headers(task, after, 100)?)
}
pub(crate) fn review(home: &Home, task: &TaskId) -> AppResult<ReviewContents> {
    let store = home.open()?;
    store.db.contract_bounded(task, CONTRACT_LIMIT)?;
    store
        .db
        .check(task, Capability::ArtifactExport, &Resource::Task)?;
    Ok(collect_review(
        &store.db,
        &store.blobs,
        task,
        Some(RESULT_LIMIT),
    )?)
}
fn latest_job(jobs_root: &std::path::Path, effect: &EffectId) -> Option<JobSummary> {
    let job = JobDir::list(jobs_root, effect)
        .ok()?
        .into_iter()
        .next_back()?;
    let request = job.request().ok()?;
    let state = job.read_status().map(|s| s.state);
    Some(JobSummary {
        effect_id: effect.clone(),
        attempt_id: request.attempt_id,
        lease_generation: request.lease_generation,
        state,
        alive: !job.is_dead(),
        receipt: job.read_receipt().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::ids::Digest;
    use serde_json::json;
    fn fixture() -> (tempfile::TempDir, Home, TaskId) {
        let root = tempfile::tempdir().unwrap();
        let home = Home::new(Some(root.path().join("home")), None).unwrap();
        let c=Contract::parse(&json!({
            "goal":"fix the parser","repository":{"source":"/repo","revision":"abc"},
            "profile":"python-stdlib-v1","verification_profile":"parser-checks-v1","editable_paths":["src/**"],
            "capabilities":["snapshot.read","artifact.export"],
            "limits":{"model_requests":1,"max_output_tokens_per_request":1000,"tool_actions":10,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}
        }).to_string()).unwrap();
        let store = home.open().unwrap();
        let id = store
            .db
            .create_task(&c, &Digest::of(&serde_json::to_vec(&c).unwrap()))
            .unwrap();
        store.db.append_audit(&id,"Submitted",&json!({"worker":"host","model":"fake:/fixture","repository_digest":Digest::of(b"repo"),"profile_digest":Digest::of(b"profile"),"model_policy_version":1,"model_limits_version":1})).unwrap();
        (root, home, id)
    }
    #[test]
    fn shared_queries_use_recorded_metadata_without_writing_or_exposing_handles() {
        let (_root, home, id) = fixture();
        let before = home.open().unwrap().db.events(&id).unwrap();
        let s = status(&home, &id, Some(RESULT_LIMIT)).unwrap();
        let shown = serde_json::to_value(&s).unwrap();
        assert_eq!(shown["state"], "READY");
        assert_eq!(shown["model"], "fake:/fixture");
        assert_eq!(shown["worker"], "host");
        assert_eq!(shown["model_policy_version"], 1);
        assert_eq!(shown["jobs"], json!([]));
        assert!(shown["model_endpoint"].is_null());
        assert!(!shown.as_object().unwrap().contains_key("guest_image"));
        assert!(!shown.as_object().unwrap().contains_key("jailed"));
        assert_eq!(
            tasks(&home, None, None).unwrap().rows[0].model,
            "fake:/fixture"
        );
        assert_eq!(detail(&home, &id).unwrap().contract.goal, "fix the parser");
        assert_eq!(events(&home, &id, 0).unwrap().rows.len(), 2);
        assert_eq!(
            review(&home, &id).err().unwrap().kind,
            AppErrorKind::Forbidden
        );
        assert_eq!(home.open().unwrap().db.events(&id).unwrap(), before);
    }
    #[test]
    fn driver_observation_preserves_idle_and_busy_lock_contents() {
        let (_root, home, id) = fixture();
        assert!(home.driver_status().unwrap().is_none());
        assert!(!home.root.join("driver.lock").exists());
        let lock = home.lock().unwrap();
        lock.driving(&id).unwrap();
        assert_eq!(home.driver_status().unwrap(), Some(id.clone()));
        assert_eq!(
            std::fs::read_to_string(home.root.join("driver.lock")).unwrap(),
            id.as_str()
        );
        drop(lock);
        assert!(home.driver_status().unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(home.root.join("driver.lock")).unwrap(),
            id.as_str()
        );
    }
    #[test]
    fn web_queries_guard_oversized_contract_before_legacy_reads() {
        let (_root, home, id) = fixture();
        // Use the store's DB path only inside the owned fixture home.
        let conn = rusqlite::Connection::open(home.root.join("agentos.db")).unwrap();
        conn.execute("UPDATE tasks SET contract_json=?1", ["x".repeat(300_000)])
            .unwrap();
        assert_eq!(
            status(&home, &id, Some(RESULT_LIMIT)).unwrap_err().kind,
            AppErrorKind::TooLarge
        );
        assert_eq!(detail(&home, &id).unwrap_err().kind, AppErrorKind::TooLarge);
        assert_eq!(
            review(&home, &id).err().unwrap().kind,
            AppErrorKind::TooLarge
        );
        assert_eq!(events(&home, &id, 0).unwrap().rows.len(), 2);
    }
}
