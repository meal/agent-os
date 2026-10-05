use super::{AppError, AppErrorKind, AppResult};
use crate::{
    crash::CrashSpec,
    drive::{Driver, agent_for, drive},
    home::{DriverLock, Home, Store},
};
use agentos_core::{
    effect::EffectState,
    ids::{Digest, TaskId},
    state::{TaskEvent, TaskState},
};
use agentos_engine::{recover::recover, routing::RoutingExecutor, supervised::SupervisedExecutor};
use std::path::PathBuf;
pub(crate) struct RunRequest {
    pub task: TaskId,
    pub patch: Option<PathBuf>,
    pub crash: Option<CrashSpec>,
    pub reviewed_contract: Option<Digest>,
}
#[derive(Clone)]
pub(crate) struct ControlOutcome {
    pub task_id: TaskId,
    pub state: TaskState,
    pub cancel_requested: bool,
    pub note: Option<String>,
}
pub(crate) struct DrivingRun {
    home: Home,
    store: Store,
    lock: DriverLock,
    task: TaskId,
    agent: Option<Driver>,
    exec: RoutingExecutor<SupervisedExecutor>,
    crash: Option<CrashSpec>,
}
pub(crate) enum PreparedRun {
    Report(ControlOutcome),
    Drive(DrivingRun),
    Reconcile(DrivingRun),
}
fn outcome(store: &Store, task: &TaskId, note: Option<String>) -> AppResult<ControlOutcome> {
    let t = store.db.task(task)?;
    Ok(ControlOutcome {
        task_id: task.clone(),
        state: t.state,
        cancel_requested: t.cancel_requested,
        note,
    })
}
fn lock(home: &Home) -> AppResult<DriverLock> {
    home.try_lock()?.ok_or_else(|| {
        AppError::new(
            AppErrorKind::Conflict,
            format!(
                "another agentos process is driving tasks in {}; try again when it finishes",
                home.root.display()
            ),
        )
    })
}
pub(crate) fn prepare(home: &Home, request: RunRequest) -> AppResult<PreparedRun> {
    let store = home.open()?;
    home.model_endpoint(&store, &request.task)?;
    agentos_engine::model::policy::versions(&store.db, &request.task)?;
    let t = store.db.task(&request.task)?;
    if t.state.is_terminal()
        && !store
            .db
            .outstanding_effects(&request.task)?
            .iter()
            .any(|e| matches!(e.state, EffectState::Intended | EffectState::Dispatched))
    {
        return Ok(PreparedRun::Report(outcome(&store, &request.task, None)?));
    }
    // CLI validates a requested patch/model before trying the driver's lock.
    if request.reviewed_contract.is_none() && !t.state.is_terminal() {
        agent_for(home, &store, &request.task, request.patch.as_deref())?;
    }
    prepare_locked(home, request, lock(home)?)
}
pub(crate) fn prepare_locked(
    home: &Home,
    request: RunRequest,
    lock: DriverLock,
) -> AppResult<PreparedRun> {
    let store = home.open()?;
    let task = request.task;
    home.task_worker(&store, &task)?;
    home.model_endpoint(&store, &task)?;
    agentos_engine::model::policy::versions(&store.db, &task)?;
    let t = store.db.task(&task)?;
    if let Some(reviewed) = &request.reviewed_contract {
        if t.state != TaskState::Ready {
            return Err(AppError::new(
                AppErrorKind::Conflict,
                "Only READY tasks can be approved",
            ));
        }
        let contract = store
            .db
            .contract_bounded(&task, super::queries::CONTRACT_LIMIT)?;
        let recorded = store
            .db
            .first_event_bounded(&task, "TaskCreated", Some(64 * 1024))?
            .ok_or_else(|| AppError::new(AppErrorKind::Conflict, "Recorded contract is missing"))?;
        if Digest::of(&serde_json::to_vec(&contract)?) != *reviewed
            || recorded.payload["contract_digest"].as_str() != Some(&reviewed.to_string())
        {
            return Err(AppError::new(
                AppErrorKind::Conflict,
                "The contract changed; review the recorded inputs again",
            ));
        }
        if let Some(reason) = crate::drive::input_problem(home, &store, &task)? {
            return Err(AppError::new(AppErrorKind::Conflict, reason));
        }
    }
    if t.state.is_terminal() {
        let exec = home.recovery_executor(&store, &task)?;
        lock.driving(&task)?;
        return Ok(PreparedRun::Reconcile(DrivingRun {
            home: home.clone(),
            store,
            lock,
            task,
            agent: None,
            exec,
            crash: request.crash,
        }));
    }
    let agent = agent_for(home, &store, &task, request.patch.as_deref())?;
    let exec = home.executor(&store, &task)?;
    let t = store.db.task(&task)?;
    if t.state == TaskState::Paused && !t.cancel_requested {
        store.db.append(&task, &TaskEvent::Resumed)?;
    }
    if t.state == TaskState::Ready && !t.cancel_requested {
        store.db.approve_task(&task)?;
    }
    Ok(PreparedRun::Drive(DrivingRun {
        home: home.clone(),
        store,
        lock,
        task,
        agent: Some(agent),
        exec,
        crash: request.crash,
    }))
}
pub(crate) async fn drive_prepared(prepared: PreparedRun) -> AppResult<ControlOutcome> {
    match prepared {
        PreparedRun::Report(result) => Ok(result),
        PreparedRun::Drive(r) => {
            drive(
                &r.home,
                &r.store,
                &r.lock,
                &r.task,
                r.agent.expect("driving run has an agent"),
                r.crash.as_ref(),
                r.exec,
            )
            .await?;
            outcome(&r.store, &r.task, None)
        }
        PreparedRun::Reconcile(r) => {
            recover(&r.store.db, &r.store.blobs, &r.exec, &r.task)
                .await
                .map_err(crate::error::CliError::from)?;
            outcome(&r.store, &r.task, None)
        }
    }
}
pub(crate) fn pause(home: &Home, task: &TaskId) -> AppResult<ControlOutcome> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    match t.state {
        TaskState::Paused => {}
        TaskState::Running | TaskState::Waiting => {
            store.db.append(task, &TaskEvent::Paused).map_err(|e| {
                AppError::new(
                    AppErrorKind::Conflict,
                    format!("cannot pause task {task}: {e}"),
                )
            })?;
        }
        other => {
            return Err(AppError::new(
                AppErrorKind::Conflict,
                format!("cannot pause task {task}: it is {}", other.label()),
            ));
        }
    }
    outcome(&store, task, None)
}
pub(crate) async fn cancel(home: &Home, task: &TaskId) -> AppResult<ControlOutcome> {
    let store = home.open()?;
    let t = store.db.task(task)?;
    if t.state.is_terminal() {
        return outcome(
            &store,
            task,
            Some("already finished; nothing to cancel".into()),
        );
    }
    if !t.cancel_requested {
        store.db.append(task, &TaskEvent::CancelRequested)?;
    }
    crate::commands::revoke::cancel_running_jobs(home, &store, task, None)?;
    let Some(lock) = home.try_lock()? else {
        let note = if home.driven_task().as_deref() == Some(task.as_str()) {
            "another agentos process is driving this task; it completes the cancel at its next step"
                .to_owned()
        } else {
            format!(
                "another agentos process is driving tasks; the cancel completes on the next `agentos resume {task}` or `agentos cancel {task}`"
            )
        };
        return outcome(&store, task, Some(note));
    };
    lock.driving(task)?;
    let t = store.db.task(task)?;
    if !t.state.is_terminal() {
        if store.db.outstanding_effects(task)?.is_empty() {
            store.db.append(task, &TaskEvent::CancelCompleted)?;
        } else {
            let exec=home.recovery_executor(&store,task).map_err(|e|AppError::from(crate::error::CliError{code:e.code,message:format!("{e}; the cancel is requested and completes on a later `agentos resume {task}` or `agentos cancel {task}`")}))?;
            recover(&store.db, &store.blobs, &exec, task)
                .await
                .map_err(crate::error::CliError::from)?;
        }
    }
    outcome(&store, task, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        queries,
        submission::{self, CreateRequest},
    };
    fn fixture() -> (tempfile::TempDir, Home, TaskId) {
        let root = tempfile::tempdir().unwrap();
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let home = Home::new(
            Some(root.path().join("home")),
            Some(fixtures.join("profiles")),
        )
        .unwrap();
        let c = serde_json::json!({"goal":"fix parser","repository":{"source":fixtures.join("parser-repo"),"revision":"recorded-at-submission"},"profile":"python-stdlib-v1","verification_profile":"parser-checks-v1","editable_paths":["src/**"],"capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export"],"limits":{"model_requests":1,"max_output_tokens_per_request":1000,"tool_actions":10,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}});
        let r = submission::create(
            &home,
            CreateRequest {
                contract_json: c.to_string(),
                worker: crate::args::WorkerKind::Host,
                model: None,
                patch: Some(fixtures.join("parser-repo.fix.patch")),
            },
        )
        .unwrap();
        (root, home, r.task_id)
    }
    #[test]
    fn reviewed_start_refuses_tampering_before_capabilities_or_journal_writes() {
        let (_root, home, id) = fixture();
        let detail = queries::detail(&home, &id).unwrap();
        let store = home.open().unwrap();
        let before = store.db.events(&id).unwrap();
        for reviewed in [Digest::of(b"wrong"), detail.contract_digest] {
            if reviewed == detail.contract_digest {
                std::fs::write(home.task_dir(&id).join("profile/extra.py"), b"changed").unwrap();
            }
            let error = prepare(
                &home,
                RunRequest {
                    task: id.clone(),
                    patch: None,
                    crash: None,
                    reviewed_contract: Some(reviewed),
                },
            )
            .err()
            .unwrap();
            assert_eq!(error.kind, AppErrorKind::Conflict);
            assert_eq!(store.db.task(&id).unwrap().state, TaskState::Ready);
            assert!(store.db.grants(&id).unwrap().is_empty());
            assert_eq!(store.db.events(&id).unwrap(), before);
        }
    }
    #[test]
    fn typed_pause_and_terminal_report_follow_durable_state_without_worker_or_key() {
        let (_root, mut home, id) = fixture();
        let store = home.open().unwrap();
        store.db.approve_task(&id).unwrap();
        store.db.append(&id, &TaskEvent::Started).unwrap();
        let paused = pause(&home, &id).unwrap();
        assert_eq!(paused.state, TaskState::Paused);
        assert!(!paused.cancel_requested);
        assert!(paused.note.is_none());
        store.db.append(&id, &TaskEvent::CancelRequested).unwrap();
        store.db.append(&id, &TaskEvent::CancelCompleted).unwrap();
        let before = store.db.events(&id).unwrap();
        home.api_key_file = Some(home.root.join("missing-key"));
        let report = prepare(
            &home,
            RunRequest {
                task: id.clone(),
                patch: None,
                crash: None,
                reviewed_contract: None,
            },
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
        assert!(matches!(report, PreparedRun::Report(_)));
        assert_eq!(store.db.events(&id).unwrap(), before);
        assert!(!home.root.join("driver.lock").exists());
    }
}
