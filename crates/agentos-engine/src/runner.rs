//! The run loop. Each effect goes through separate steps, in this order, so recovery can
//! resume between any two of them:
//! [`intend`] -> [`dispatch`] -> [`execute`] -> [`publish`] -> [`complete`].

use agentos_core::budget::Reservation;
use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectRecord, EffectState, Outcome, ReceiptVerdict};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{Task, TaskEvent, TaskState};
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError};
use serde_json::json;

use crate::agent::{Agent, AgentAction, Observation};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::patch::patch_paths;

pub const WORKER: &str = "fixture-executor";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("blob store: {0}")]
    Blob(#[from] std::io::Error),
    #[error("task {task} has outstanding effects {effects:?}; recover them before running")]
    OutstandingEffects { task: TaskId, effects: Vec<EffectId> },
    #[error("effect {effect} is {state:?}, expected it to be freshly intended")]
    UnexpectedEffectState { effect: EffectId, state: EffectState },
    #[error("receipt for effect {effect} was not applied: {verdict:?}")]
    ReceiptNotApplied { effect: EffectId, verdict: ReceiptVerdict },
    #[error("executor protocol violation: {0}")]
    Protocol(String),
}

type Result<T> = std::result::Result<T, EngineError>;

/// Where the loop goes after a step: give the agent an observation, or stop.
enum Next {
    Observe(Observation),
    Stop(TaskState),
}

/// Ceiling on agent turns per `run_task` call. Denied patches and repeated verifications
/// consume no tool action, so without it a confused agent could loop forever.
fn turn_limit(contract: &Contract) -> u32 {
    contract.limits.tool_actions.saturating_mul(4).saturating_add(8)
}

/// Step 1: durably record the intent (and its reservation) before anything runs.
pub fn intend(
    db: &Db,
    task: &TaskId,
    kind: EffectKind,
    request: Digest,
    expected_workspace: &Digest,
) -> Result<EffectRecord> {
    let reserve = Reservation::for_kind(&kind, 0);
    let rec = db.record_intent(task, kind, request, expected_workspace, reserve)?;
    tracing::info!(
        task_id = %task, step = rec.step, effect_id = %rec.effect_id, kind = rec.kind.tag(),
        "effect intended"
    );
    Ok(rec)
}

/// Step 2: mark the effect dispatched under a fresh attempt and the next lease generation.
pub fn dispatch(db: &Db, rec: &EffectRecord) -> Result<AttemptCtx> {
    let ctx = AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: rec.lease_generation + 1,
        worker: WORKER.to_string(),
    };
    db.mark_dispatched(&rec.effect_id, &ctx.attempt_id, &ctx.worker, ctx.lease_generation)?;
    tracing::info!(
        task_id = %rec.task_id, effect_id = %rec.effect_id, lease = ctx.lease_generation,
        "effect dispatched"
    );
    Ok(ctx)
}

/// Step 3: run the attempt on the executor.
pub async fn execute<E: Executor>(
    executor: &E,
    rec: &EffectRecord,
    payload: Vec<u8>,
    contract: &Contract,
    ctx: &AttemptCtx,
) -> Result<ExecOutcome> {
    let req = EffectRequest {
        effect_id: rec.effect_id.clone(),
        task_id: rec.task_id.clone(),
        kind: rec.kind.clone(),
        payload,
        contract: contract.clone(),
    };
    let out = executor.run(&req, ctx).await;
    if out.receipt.result_digest != Some(Digest::of(&out.output)) {
        let msg = format!("receipt for {} does not describe its output", rec.effect_id);
        return Err(EngineError::Protocol(msg));
    }
    tracing::info!(
        task_id = %rec.task_id, effect_id = %rec.effect_id, outcome = ?out.receipt.outcome,
        "effect executed"
    );
    Ok(out)
}

fn artifact_type(kind: &EffectKind, outcome: &Outcome) -> &'static str {
    match (kind, outcome) {
        (_, Outcome::Failure(_)) => "effect-failure",
        (EffectKind::ReadSnapshot, _) => "snapshot-manifest",
        (EffectKind::ApplyPatch { .. }, _) => "patch-result",
        (EffectKind::RunVerification, _) => "verification-evidence",
        (EffectKind::ExportBundle, _) => "export-bundle",
    }
}

/// Step 4: write the result bytes to the blob store, then register them for the effect.
pub fn publish(
    db: &Db,
    blobs: &BlobStore,
    rec: &EffectRecord,
    ctx: &AttemptCtx,
    out: &ExecOutcome,
) -> Result<Digest> {
    let digest = blobs.put(&out.output)?;
    let provenance = json!({ "worker": ctx.worker, "attempt_id": ctx.attempt_id, "effect_id": rec.effect_id });
    db.register_artifact(
        &digest,
        out.output.len() as u64,
        artifact_type(&rec.kind, &out.receipt.outcome),
        Some(&rec.effect_id),
        &provenance.to_string(),
    )?;
    Ok(digest)
}

/// Step 5: apply the receipt together with the follow-up task event.
pub fn complete(
    db: &Db,
    rec: &EffectRecord,
    out: &ExecOutcome,
    artifact: &Digest,
    follow_up: Option<TaskEvent>,
) -> Result<()> {
    let verdict = db.complete_effect(&rec.effect_id, &out.receipt, Some(artifact), follow_up)?;
    if verdict != ReceiptVerdict::Apply {
        return Err(EngineError::ReceiptNotApplied { effect: rec.effect_id.clone(), verdict });
    }
    tracing::info!(task_id = %rec.task_id, effect_id = %rec.effect_id, "effect completed");
    Ok(())
}

/// Runs steps 2-5 for a freshly intended effect. `follow_up` picks the task event from the
/// outcome and the task as it is right before completion.
async fn run_effect<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    contract: &Contract,
    rec: &EffectRecord,
    payload: Vec<u8>,
    follow_up: impl FnOnce(&ExecOutcome, &Task) -> Option<TaskEvent>,
) -> Result<ExecOutcome> {
    if rec.state != EffectState::Intended {
        return Err(EngineError::UnexpectedEffectState { effect: rec.effect_id.clone(), state: rec.state });
    }
    let ctx = dispatch(db, rec)?;
    let out = execute(executor, rec, payload, contract, &ctx).await?;
    let artifact = publish(db, blobs, rec, &ctx, &out)?;
    let task = db.task(&rec.task_id)?;
    let event = follow_up(&out, &task);
    complete(db, rec, &out, &artifact, event)?;
    Ok(out)
}

/// A `WorkspaceUpdated` follow-up for a successful workspace change, if the task can take it.
fn workspace_update(out: &ExecOutcome, task: &Task) -> Option<TaskEvent> {
    match (&out.receipt.outcome, out.new_workspace) {
        (Outcome::Success, Some(digest)) if task.state == TaskState::Running && !task.cancel_requested => {
            Some(TaskEvent::WorkspaceUpdated { digest })
        }
        _ => None,
    }
}

fn fail(db: &Db, task: &TaskId, reason: &str) -> Result<TaskState> {
    tracing::info!(task_id = %task, reason, "task failed");
    Ok(db.append(task, &TaskEvent::Failed { reason: reason.into() })?.state)
}

/// Terminal, paused or waiting tasks stop the loop; a pending cancel completes here, since
/// no work is in flight between steps.
fn interrupted(db: &Db, task: &TaskId) -> Result<Option<TaskState>> {
    let t = db.task(task)?;
    if t.state.is_terminal() {
        return Ok(Some(t.state));
    }
    if t.cancel_requested {
        tracing::info!(task_id = %task, "cancel completed");
        return Ok(Some(db.append(task, &TaskEvent::CancelCompleted)?.state));
    }
    if matches!(t.state, TaskState::Paused | TaskState::Waiting) {
        return Ok(Some(t.state));
    }
    Ok(None)
}

/// The task's ReadSnapshot effect, found through its `EffectIntended` journal entry.
fn snapshot_effect(db: &Db, task: &TaskId) -> Result<Option<EffectRecord>> {
    for ev in db.events(task)? {
        if ev.event_type == "EffectIntended" && ev.payload["kind"] == "ReadSnapshot" {
            let id: EffectId =
                serde_json::from_value(ev.payload["effect_id"].clone()).map_err(DbError::from)?;
            return Ok(Some(db.effect(&id)?));
        }
    }
    Ok(None)
}

/// Ensures the workspace exists; returns the snapshot's file list, or the state to stop in.
async fn ensure_snapshot<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    contract: &Contract,
    task: &TaskId,
) -> Result<std::result::Result<Vec<String>, TaskState>> {
    let rec = match snapshot_effect(db, task)? {
        Some(rec) => rec,
        None => {
            let t = db.task(task)?;
            let request = Digest::of(contract.repository.revision.as_bytes());
            let rec = intend(db, task, EffectKind::ReadSnapshot, request, &t.workspace_digest)?;
            run_effect(db, blobs, executor, contract, &rec, Vec::new(), workspace_update).await?;
            db.effect(&rec.effect_id)?
        }
    };
    if rec.state != EffectState::Completed {
        return Ok(Err(fail(db, task, "snapshot failed")?));
    }
    let digest = rec.result_digest.ok_or_else(|| EngineError::Protocol("snapshot without manifest".into()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&blobs.get(&digest)?).map_err(DbError::from)?;
    let files = serde_json::from_value(manifest["files"].clone()).map_err(DbError::from)?;
    Ok(Ok(files))
}

async fn apply_patch<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    contract: &Contract,
    task: &TaskId,
    base: Digest,
    patch: String,
) -> Result<Next> {
    let request = Digest::of(patch.as_bytes());
    // Broker pre-check: refused patches create no effect and consume no tool action.
    let denied = match patch_paths(&patch).await {
        Err(detail) => Some(json!({
            "action": "ApplyPatch", "reason": "InvalidPatch", "detail": detail, "request_digest": request,
        })),
        Ok(paths) => {
            let refused: Vec<&String> = paths.iter().filter(|p| !contract.path_allowed(p)).collect();
            (!refused.is_empty()).then(|| {
                json!({
                    "action": "ApplyPatch", "reason": "PathNotEditable", "paths": refused, "request_digest": request,
                })
            })
        }
    };
    if let Some(audit) = denied {
        db.append_audit(task, "Denied", &audit)?;
        tracing::info!(task_id = %task, %audit, "patch denied");
        return Ok(Next::Observe(Observation::PatchRejected { reason: audit.to_string() }));
    }

    // Publish the patch itself first so the intent's request digest names a stored blob.
    blobs.put(patch.as_bytes())?;
    let kind = EffectKind::ApplyPatch { expected_base: base };
    let rec = match intend(db, task, kind, request, &base) {
        Ok(rec) => rec,
        Err(EngineError::Db(DbError::VersionConflict { expected, actual })) => {
            return Ok(Next::Observe(Observation::VersionConflict { expected, actual }));
        }
        Err(EngineError::Db(DbError::BudgetExceeded(_))) => {
            return Ok(Next::Stop(fail(db, task, "budget exhausted")?));
        }
        Err(EngineError::Db(DbError::CapabilityDenied(cap))) => {
            return Ok(Next::Observe(Observation::PatchRejected { reason: format!("capability {cap:?} not granted") }));
        }
        Err(e) => return Err(e),
    };
    let provenance = json!({ "source": "agent" }).to_string();
    db.register_artifact(&request, patch.len() as u64, "patch", Some(&rec.effect_id), &provenance)?;
    if rec.state == EffectState::Failed {
        // Same patch on the same base already failed; the store returned that record.
        return Ok(Next::Observe(Observation::PatchRejected { reason: "this patch already failed to apply".into() }));
    }
    let out = run_effect(db, blobs, executor, contract, &rec, patch.into_bytes(), workspace_update).await?;
    Ok(Next::Observe(match (&out.receipt.outcome, out.new_workspace) {
        (Outcome::Success, Some(workspace)) => Observation::PatchApplied { workspace },
        (Outcome::Success, None) => {
            return Err(EngineError::Protocol("patch applied without a workspace digest".into()));
        }
        (Outcome::Failure(reason), _) => Observation::PatchRejected { reason: reason.clone() },
    }))
}

/// Verifies the current workspace. `started` is true when resuming a task already in
/// VERIFYING, so `VerifyStarted` is not appended twice.
async fn verify<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    contract: &Contract,
    task: &TaskId,
    started: bool,
) -> Result<Next> {
    let t = if started { db.task(task)? } else { db.append(task, &TaskEvent::VerifyStarted)? };
    tracing::info!(task_id = %task, step = t.step, "verification started");
    let workspace = t.workspace_digest;
    let rec = intend(db, task, EffectKind::RunVerification, Digest::of(workspace.as_bytes()), &workspace)?;
    let mut verdict = (false, String::new());
    let decide = |out: &ExecOutcome, now: &Task| {
        verdict = match (&out.receipt.outcome, &out.verification) {
            // Evidence counts only for the digest the task holds right now.
            (Outcome::Success, Some(r)) if r.workspace == now.workspace_digest => (r.passed, r.summary.clone()),
            (Outcome::Success, Some(r)) => {
                (false, format!("evidence is for workspace {}, task has {}", r.workspace, now.workspace_digest))
            }
            (Outcome::Success, None) => (false, "executor returned no verification report".into()),
            (Outcome::Failure(reason), _) => (false, format!("verification did not complete: {reason}")),
        };
        if now.state != TaskState::Verifying || now.cancel_requested {
            return None;
        }
        Some(if verdict.0 {
            TaskEvent::VerifyPassed { digest: now.workspace_digest }
        } else {
            TaskEvent::VerifyFailed
        })
    };
    run_effect(db, blobs, executor, contract, &rec, Vec::new(), decide).await?;
    let (passed, summary) = verdict;
    tracing::info!(task_id = %task, passed, "verification finished");
    Ok(Next::Observe(Observation::Verification { passed, summary }))
}

fn finish(db: &Db, task: &TaskId) -> Result<TaskState> {
    let t = db.task(task)?;
    if t.state == TaskState::Succeeded {
        return Ok(t.state);
    }
    fail(db, task, "agent finished without verified success")
}

fn observed_workspace(obs: &Observation) -> Option<Digest> {
    match obs {
        Observation::Start { workspace, .. } | Observation::PatchApplied { workspace } => Some(*workspace),
        Observation::VersionConflict { actual, .. } => Some(*actual),
        _ => None,
    }
}

/// Drives `task` until it is terminal, paused or waiting. Resumable: a RUNNING or VERIFYING
/// task continues from the journal (the snapshot is not taken twice); a terminal task is
/// returned untouched; a PAUSED one is left for the caller to resume with `Resumed`.
/// Outstanding effects from a crashed run are an error until recovery handles them.
pub async fn run_task<E: Executor, A: Agent>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    agent: &mut A,
    task: &TaskId,
) -> std::result::Result<TaskState, EngineError> {
    let contract = db.contract(task)?;
    if let Some(state) = interrupted(db, task)? {
        return Ok(state);
    }
    let outstanding = db.outstanding_effects(task)?;
    if !outstanding.is_empty() {
        let effects = outstanding.into_iter().map(|e| e.effect_id).collect();
        return Err(EngineError::OutstandingEffects { task: task.clone(), effects });
    }
    if db.task(task)?.state == TaskState::Ready {
        db.append(task, &TaskEvent::Started)?;
        tracing::info!(task_id = %task, "task started");
    }
    let files = match ensure_snapshot(db, blobs, executor, &contract, task).await? {
        Ok(files) => files,
        Err(state) => return Ok(state),
    };

    let t = db.task(task)?;
    let mut base = t.workspace_digest;
    let mut next = if t.state == TaskState::Verifying {
        verify(db, blobs, executor, &contract, task, true).await?
    } else {
        Next::Observe(Observation::Start { files, workspace: base })
    };
    let mut turns = 0;
    loop {
        let obs = match next {
            Next::Observe(obs) => obs,
            Next::Stop(state) => return Ok(state),
        };
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        if turns >= turn_limit(&contract) {
            return fail(db, task, "agent turn limit exceeded");
        }
        turns += 1;
        base = observed_workspace(&obs).unwrap_or(base);
        let action = agent.next(&obs);
        // A pause or cancel that arrived while the agent was deciding wins over its action.
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        next = match action {
            AgentAction::ApplyPatch(patch) => apply_patch(db, blobs, executor, &contract, task, base, patch).await?,
            AgentAction::Verify => verify(db, blobs, executor, &contract, task, false).await?,
            AgentAction::Finish => Next::Stop(finish(db, task)?),
        };
    }
}
