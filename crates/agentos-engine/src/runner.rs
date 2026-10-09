//! The run loop. The agent chooses an action, the runner journals it as an `AgentTurn`, then
//! performs it through the effect steps in [`crate::steps`]. Every action is idempotent
//! against the journal (see [`crate::journal`]), so after a crash `run_task` recovers the
//! outstanding effects ([`crate::recover`]), replays the open session into a fresh agent
//! and carries on exactly where the killed controller stopped.

use agentos_core::broker::Resource;
use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{
    AgentSessionSpec, EffectId, EffectKind, EffectRecord, EffectState, ReceiptVerdict,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::messages::normalize_request;
use agentos_core::state::{TaskEvent, TaskState, TransitionError};
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError};
use serde_json::json;

use crate::agent::{Agent, AgentAction, Observation};
use crate::crash::{CrashPoint, RunOptions};
use crate::executor::Executor;
use crate::guestlink::guest_text;
use crate::journal;
use crate::patch::patch_paths;
use crate::recover::{self, deadline_stop};
use crate::shadow::check_path;
use crate::steps::{Attempt, Cx, intend, run_attempt};
use crate::workspace::has_excluded_component;

use crate::model::policy::{ModelFailure, ModelFailureClass, backoff_seconds, check_request_size};
pub use crate::steps::{WORKER, follow_up_event, verification_verdict};
use agentos_store::effects::ModelRetrySchedule;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("blob store: {0}")]
    Blob(#[from] std::io::Error),
    #[error("effect {effect} is {state:?}, expected it to be freshly intended or finished")]
    UnexpectedEffectState {
        effect: EffectId,
        state: EffectState,
    },
    #[error("receipt for effect {effect} was not applied: {verdict:?}")]
    ReceiptNotApplied {
        effect: EffectId,
        verdict: ReceiptVerdict,
    },
    #[error("executor protocol violation: {0}")]
    Protocol(String),
    #[error("injected crash at {0}")]
    Crashed(CrashPoint),
    #[error("agent replay diverged at turn {turn}: journaled {journaled}, agent chose {emitted}")]
    NondeterministicAgent {
        turn: u32,
        journaled: String,
        emitted: String,
    },
}

pub(crate) type Result<T> = std::result::Result<T, EngineError>;

/// Where the loop goes after a step: give the agent an observation, or stop.
enum Next {
    Observe(Observation),
    Stop(TaskState),
}

/// Ceiling on agent turns per session. Denied patches and repeated verifications consume
/// no tool action, so without it a confused agent could loop forever.
fn turn_limit(contract: &Contract) -> u32 {
    contract
        .limits
        .tool_actions
        .saturating_mul(4)
        .saturating_add(8)
}

/// The capability's contract name, e.g. `verification.run`.
pub(crate) fn capability_name(cap: Capability) -> String {
    serde_json::to_value(cap)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub(crate) fn fail(db: &Db, task: &TaskId, reason: &str) -> Result<TaskState> {
    tracing::info!(task_id = %task, reason, "task failed");
    Ok(db
        .append(
            task,
            &TaskEvent::Failed {
                reason: reason.into(),
            },
        )?
        .state)
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

/// A pause or cancel can land between the runner's [`interrupted`] check and its next write;
/// the write is then refused (not dispatchable, or the reducer rejects `VerifyStarted`).
/// Such a refusal stops the run as the interrupt says; any other error propagates. Nothing
/// is lost: an intent left behind is dispatched on resume or abandoned once cancelled.
fn or_interrupted<T>(
    db: &Db,
    task: &TaskId,
    r: Result<T>,
) -> Result<std::result::Result<T, TaskState>> {
    let refused = matches!(
        &r,
        Err(EngineError::Db(
            DbError::NotDispatchable { .. }
                | DbError::Transition(
                    TransitionError::CancelRequested { .. }
                        | TransitionError::Terminal(_)
                        | TransitionError::InvalidTransition { .. }
                )
        ))
    );
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(e) if refused => match interrupted(db, task)? {
            Some(state) => {
                tracing::info!(task_id = %task, error = %e, ?state, "interrupted before the next write");
                Ok(Err(state))
            }
            None => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// A step's result, with an interrupt-refused write turned into a stop.
fn interruptible(db: &Db, task: &TaskId, r: Result<Next>) -> Result<Next> {
    Ok(or_interrupted(db, task, r)?.unwrap_or_else(Next::Stop))
}

/// Runs a freshly intended effect, or takes an already finished one as it is (an idempotent
/// intent may return it), and gives the agent its observation.
async fn effect_turn<E: Executor>(
    cx: &Cx<'_, E>,
    rec: EffectRecord,
    payload: Vec<u8>,
) -> Result<Next> {
    let rec = match rec.state {
        EffectState::Intended => match run_attempt(cx, &rec, payload).await? {
            Attempt::Published(ReceiptVerdict::Apply) => cx.db.effect(&rec.effect_id)?,
            Attempt::Published(verdict) => {
                return Err(EngineError::ReceiptNotApplied {
                    effect: rec.effect_id,
                    verdict,
                });
            }
            Attempt::Ended(state) => return Ok(Next::Stop(state)),
            Attempt::Forfeited => cx.db.effect(&rec.effect_id)?,
        },
        EffectState::Completed | EffectState::Failed => rec,
        state => {
            return Err(EngineError::UnexpectedEffectState {
                effect: rec.effect_id,
                state,
            });
        }
    };
    Ok(Next::Observe(journal::effect_observation(cx.blobs, &rec)?))
}

/// Ensures the workspace exists; returns the snapshot's file list and whether the snapshot
/// predates this call, or the state to stop in.
async fn ensure_snapshot<E: Executor>(
    cx: &Cx<'_, E>,
) -> Result<std::result::Result<(Vec<String>, bool), TaskState>> {
    let (db, task) = (cx.db, &cx.task);
    let events = db.events(task)?;
    let (rec, resumed) = match journal::intended(&events, "ReadSnapshot", None)? {
        Some(id) => (db.effect(&id)?, true),
        None => {
            if let Some(state) = deadline_stop(cx).await? {
                return Ok(Err(state));
            }
            if let Err(denial) = granted(db, task, Capability::SnapshotRead, &Resource::Task)? {
                tracing::info!(task_id = %task, denial, "snapshot denied by the broker");
                let reason = format!(
                    "capability {} not granted",
                    capability_name(Capability::SnapshotRead)
                );
                return Ok(Err(fail(db, task, &reason)?));
            }
            let t = db.task(task)?;
            let request = Digest::of(cx.contract.repository.revision.as_bytes());
            let rec = intend(
                db,
                task,
                EffectKind::ReadSnapshot,
                request,
                &t.workspace_digest,
                &Resource::Task,
            )?;
            cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
            match run_attempt(cx, &rec, Vec::new()).await? {
                Attempt::Published(ReceiptVerdict::Apply) => (db.effect(&rec.effect_id)?, false),
                Attempt::Published(verdict) => {
                    return Err(EngineError::ReceiptNotApplied {
                        effect: rec.effect_id,
                        verdict,
                    });
                }
                Attempt::Ended(state) => return Ok(Err(state)),
                Attempt::Forfeited => {
                    return Err(EngineError::Protocol(
                        "a snapshot read cannot be forfeited".into(),
                    ));
                }
            }
        }
    };
    if rec.state != EffectState::Completed {
        return Ok(Err(fail(db, task, "snapshot failed")?));
    }
    let digest = rec
        .result_digest
        .ok_or_else(|| EngineError::Protocol("snapshot without manifest".into()))?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&cx.blobs.get(&digest)?).map_err(DbError::from)?;
    let files = serde_json::from_value(manifest["files"].clone()).map_err(DbError::from)?;
    Ok(Ok((files, resumed)))
}

/// On resume the workspace must still be the one the journal describes. After a host
/// restart it may be gone; nothing can be reconciled then, so the task fails once, cleanly.
fn workspace_lost<E: Executor>(cx: &Cx<'_, E>) -> Result<Option<TaskState>> {
    let expected = cx.db.task(&cx.task)?.workspace_digest;
    let reason = match cx.exec.current_workspace(&cx.task) {
        None => return Ok(None),
        Some(Ok(actual)) if actual == expected => return Ok(None),
        Some(Ok(actual)) => format!("workspace lost: expected {expected}, found {actual}"),
        Some(Err(e)) => format!("workspace lost: {e}"),
    };
    Ok(Some(fail(cx.db, &cx.task, &reason)?))
}

/// Publishes the patch text and links it to its effect, once.
fn ensure_patch_artifact<E>(cx: &Cx<'_, E>, rec: &EffectRecord, patch: &str) -> Result<()> {
    if !cx.blobs.exists(&rec.request_digest) {
        cx.blobs.put(patch.as_bytes())?;
    }
    let provenance = json!({ "source": "agent" }).to_string();
    cx.db.register_artifact(
        &rec.request_digest,
        patch.len() as u64,
        "patch",
        Some(&rec.effect_id),
        &provenance,
    )?;
    Ok(())
}

/// Recovery's view of [`ensure_patch_artifact`]: the patch text for an ApplyPatch effect,
/// from the agent turn that chose it (or its blob), published again if it was collected.
pub(crate) fn recovered_patch<E>(cx: &Cx<'_, E>, rec: &EffectRecord) -> Result<Option<String>> {
    let text = match journal::journaled_patch(cx.db, &cx.task, &rec.request_digest)? {
        Some(text) => text,
        None if cx.blobs.exists(&rec.request_digest) => {
            String::from_utf8(cx.blobs.get(&rec.request_digest)?)
                .map_err(|e| EngineError::Protocol(e.to_string()))?
        }
        None => return Ok(None),
    };
    ensure_patch_artifact(cx, rec, &text)?;
    Ok(Some(text))
}

/// The broker's pure pre-check (nothing journaled): `Err(reason)` when `op` on `resource`
/// would be denied. Any other store error propagates.
fn granted(
    db: &Db,
    task: &TaskId,
    op: Capability,
    resource: &Resource,
) -> Result<std::result::Result<(), String>> {
    match db.check(task, op, resource) {
        Ok(()) => Ok(Ok(())),
        Err(DbError::CapabilityDenied { reason, .. }) => Ok(Err(reason)),
        Err(e) => Err(e.into()),
    }
}

/// The broker pre-check: refused patches create no effect and consume no tool action.
/// Returns the patch's paths (the resource its intent is authorized on), or the denial.
async fn patch_denial(
    contract: &Contract,
    patch: &str,
    request: Digest,
) -> std::result::Result<Vec<String>, serde_json::Value> {
    let paths = patch_paths(patch).await.map_err(|detail| {
        json!({ "action": "ApplyPatch", "reason": "InvalidPatch", "detail": detail, "request_digest": request })
    })?;
    let refused = |reason: &str, bad: Vec<&String>| {
        (!bad.is_empty()).then(|| json!({ "action": "ApplyPatch", "reason": reason, "paths": bad, "request_digest": request }))
    };
    let denial = refused(
        "PathNotEditable",
        paths.iter().filter(|p| !contract.path_allowed(p)).collect(),
    )
    .or_else(|| {
        refused(
            "DigestExcludedPath",
            paths.iter().filter(|p| has_excluded_component(p)).collect(),
        )
    });
    match denial {
        Some(audit) => Err(audit),
        None => Ok(paths),
    }
}

/// Applies `patch` for the turn journaled at `since`. Idempotent: an effect or denial the
/// turn already produced is reused (see [`crate::journal`]).
async fn apply_patch<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    base: Digest,
    patch: String,
) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    let request = Digest::of(patch.as_bytes());
    let after = journal::events_after(db, task, since)?;
    if let Some(id) = journal::intended(&after, "ApplyPatch", Some(&request))? {
        let rec = db.effect(&id)?;
        ensure_patch_artifact(cx, &rec, &patch)?;
        return effect_turn(cx, rec, patch.into_bytes()).await;
    }
    if let Some(denied) = journal::denial(&after) {
        return Ok(Next::Observe(journal::denial_observation(denied)?));
    }
    // Before the patch blob is stored: closing recovery collects unregistered blobs.
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    let paths = match patch_denial(&cx.contract, &patch, request).await {
        Ok(paths) => paths,
        Err(audit) => {
            db.append_audit(task, "Denied", &audit)?;
            tracing::info!(task_id = %task, %audit, "patch denied");
            return Ok(Next::Observe(Observation::PatchRejected {
                reason: audit.to_string(),
            }));
        }
    };

    // Publish the patch itself first so the intent's request digest names a stored blob.
    cx.blobs.put(patch.as_bytes())?;
    let kind = EffectKind::ApplyPatch {
        expected_base: base,
    };
    let rec = match intend(db, task, kind, request, &base, &Resource::Paths(paths)) {
        Ok(rec) => rec,
        Err(EngineError::Db(DbError::VersionConflict { expected, actual })) => {
            return Ok(Next::Observe(Observation::VersionConflict {
                expected,
                actual,
            }));
        }
        Err(EngineError::Db(DbError::BudgetExceeded(_))) => {
            return Ok(Next::Stop(fail(db, task, "budget exhausted")?));
        }
        Err(EngineError::Db(DbError::CapabilityDenied { capability, .. })) => {
            let reason = format!("capability {capability:?} not granted");
            return Ok(Next::Observe(Observation::PatchRejected { reason }));
        }
        Err(e) => return Err(e),
    };
    if rec.state == EffectState::Intended {
        cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    }
    ensure_patch_artifact(cx, &rec, &patch)?;
    if rec.state == EffectState::Failed {
        // Same patch on the same base already failed; the store returned that record.
        return Ok(Next::Observe(Observation::PatchRejected {
            reason: "this patch already failed to apply".into(),
        }));
    }
    effect_turn(cx, rec, patch.into_bytes()).await
}

/// Verifies the current workspace for the turn journaled at `since` (none when resuming a
/// task found VERIFYING without a journaled turn). `VerifyStarted` is appended only if the
/// task is not VERIFYING yet; an effect or denial the turn already produced is reused.
async fn verify<E: Executor>(cx: &Cx<'_, E>, since: Option<u64>) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    let after = match since {
        Some(since) => journal::events_after(db, task, since)?,
        None => Vec::new(),
    };
    if let Some(id) = journal::intended(&after, "RunVerification", None)? {
        return effect_turn(cx, db.effect(&id)?, Vec::new()).await;
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    // Checked before VERIFYING is entered, so a missing grant cannot strand the task there.
    let profile = Resource::Profile(cx.contract.verification_profile.clone());
    if let Err(denial) = granted(db, task, Capability::VerificationRun, &profile)? {
        tracing::info!(task_id = %task, denial, "verification denied by the broker");
        let capability = capability_name(Capability::VerificationRun);
        if journal::denial(&after).is_none() {
            let audit = json!({ "action": "Verify", "reason": "CapabilityDenied", "capability": capability, "denial": denial });
            db.append_audit(task, "Denied", &audit)?;
            tracing::info!(task_id = %task, %audit, "verification denied");
        }
        let summary = format!("capability {capability} not granted");
        return Ok(Next::Observe(Observation::Verification {
            passed: false,
            summary,
        }));
    }
    let t = db.task(task)?;
    let t = if t.state == TaskState::Verifying {
        t
    } else {
        db.append(task, &TaskEvent::VerifyStarted)?
    };
    tracing::info!(task_id = %task, step = t.step, "verification started");
    let workspace = t.workspace_digest;
    let rec = intend(
        db,
        task,
        EffectKind::RunVerification,
        Digest::of(workspace.as_bytes()),
        &workspace,
        &profile,
    )?;
    if rec.state == EffectState::Intended {
        cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    }
    effect_turn(cx, rec, Vec::new()).await
}

fn finish(db: &Db, task: &TaskId) -> Result<TaskState> {
    let t = db.task(task)?;
    if t.state == TaskState::Succeeded {
        return Ok(t.state);
    }
    fail(db, task, "agent finished without verified success")
}

/// Publishes the serialized model request as an artifact of type `model-request`. Before the
/// turn that names it is journaled it is registered unlinked (`effect` is `None`), so that
/// recovery's blob collection keeps it even if the controller dies before the intent; after
/// the intent the same call links it to the effect.
fn ensure_request_artifact<E>(
    cx: &Cx<'_, E>,
    effect: Option<&EffectId>,
    request: &Digest,
    body: &[u8],
) -> Result<()> {
    if !cx.blobs.exists(request) {
        cx.blobs.put(body)?;
    }
    let provenance = json!({ "source": "agent" }).to_string();
    cx.db.register_artifact(
        request,
        body.len() as u64,
        "model-request",
        effect,
        &provenance,
    )?;
    Ok(())
}

/// The bytes a `CallModel` sends. A body that does not hash to `request` (a journaled action
/// carries none) is read back from the blob store, where the request was published before the
/// turn was journaled.
fn request_body(
    blobs: &BlobStore,
    request: &Digest,
    body: Vec<u8>,
    rec: Option<&EffectRecord>,
) -> Result<Vec<u8>> {
    if Digest::of(&body) == *request {
        return Ok(body);
    }
    let stored = match rec {
        Some(rec) => journal::model_request_body(blobs, rec)?,
        None => blobs
            .exists(request)
            .then(|| blobs.get(request))
            .transpose()?,
    };
    match stored {
        Some(bytes) if Digest::of(&bytes) == *request => Ok(bytes),
        _ => Err(EngineError::Protocol(format!(
            "the model request {request} is gone"
        ))),
    }
}

/// The model name the request body carries, bounded for the effect kind.
fn model_of(body: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    value["model"]
        .as_str()
        .map_or_else(|| "unknown".to_string(), |m| m.chars().take(128).collect())
}

/// Sends the request for the turn journaled at `since`, once: the effect it already produced
/// is reused (never intended or sent again), a missing grant ends the task, an exhausted
/// budget ends it too.
async fn call_model<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    turn: u32,
    base: Digest,
    request: Digest,
    body: Vec<u8>,
) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    if cx.model_policy_version == 1
        && let Some(schedule) = pending_model_retry(cx)?
        && let Some(state) = wait_model_retry(cx, &schedule).await?
    {
        return Ok(Next::Stop(state));
    }
    let after = journal::events_after(db, task, since)?;
    if let Some(id) = journal::intended(&after, "ModelCall", Some(&request))? {
        let rec = db.effect(&id)?;
        let body = request_body(cx.blobs, &request, body, Some(&rec))?;
        ensure_request_artifact(cx, Some(&rec.effect_id), &request, &body)?;
        return effect_turn(cx, rec, body).await;
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    let not_granted = || {
        format!(
            "capability {} not granted",
            capability_name(Capability::ModelRequest)
        )
    };
    if let Err(denial) = granted(db, task, Capability::ModelRequest, &Resource::Task)? {
        let capability = capability_name(Capability::ModelRequest);
        let audit = json!({ "action": "CallModel", "reason": "CapabilityDenied", "capability": capability, "denial": denial });
        db.append_audit(task, "Denied", &audit)?;
        tracing::info!(task_id = %task, %audit, "model call denied");
        return Ok(Next::Stop(fail(db, task, &not_granted())?));
    }
    let body = request_body(cx.blobs, &request, body, None)?;
    if cx.model_limits_version == 1
        && let Err(reason) = check_request_size(body.len())
    {
        return Ok(Next::Stop(fail(db, task, reason)?));
    }
    let kind = EffectKind::ModelCall {
        model: model_of(&body),
        turn,
    };
    let rec = match intend(db, task, kind, request, &base, &Resource::Task) {
        Ok(rec) => rec,
        Err(EngineError::Db(DbError::BudgetExceeded(_))) => {
            return Ok(Next::Stop(fail(db, task, "budget exhausted")?));
        }
        Err(EngineError::Db(DbError::CapabilityDenied { .. })) => {
            return Ok(Next::Stop(fail(db, task, &not_granted())?));
        }
        Err(e) => return Err(e),
    };
    if rec.state == EffectState::Intended {
        cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    }
    ensure_request_artifact(cx, Some(&rec.effect_id), &request, &body)?;
    effect_turn(cx, rec, body).await
}

/// The intent of a read (a listing or a file), with the error mapping both share: an exhausted
/// budget ends the task, a missing grant is a rejection the agent sees. `Ok(Err(next))` is that
/// early exit.
fn intend_read(
    db: &Db,
    task: &TaskId,
    kind: EffectKind,
    request: Digest,
    base: &Digest,
) -> Result<std::result::Result<EffectRecord, Next>> {
    match intend(db, task, kind, request, base, &Resource::Task) {
        Ok(rec) => Ok(Ok(rec)),
        Err(EngineError::Db(DbError::BudgetExceeded(_))) => {
            Ok(Err(Next::Stop(fail(db, task, "budget exhausted")?)))
        }
        Err(EngineError::Db(DbError::CapabilityDenied { .. })) => {
            let reason = format!(
                "capability {} not granted",
                capability_name(Capability::SnapshotRead)
            );
            Ok(Err(Next::Observe(Observation::FileReadRejected { reason })))
        }
        Err(e) => Err(e),
    }
}

/// Lists the workspace for the turn journaled at `since`, as a read effect.
async fn list_files<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    turn: u32,
    base: Digest,
) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    let after = journal::events_after(db, task, since)?;
    if let Some(id) = journal::intended(&after, "ListFiles", None)? {
        return effect_turn(cx, db.effect(&id)?, Vec::new()).await;
    }
    if let Some(denied) = journal::denial(&after) {
        return Ok(Next::Observe(journal::denial_observation(denied)?));
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    let rec = match intend_read(
        db,
        task,
        EffectKind::ListFiles { turn },
        Digest::of(b"list_files"),
        &base,
    )? {
        Ok(rec) => rec,
        Err(next) => return Ok(next),
    };
    if rec.state == EffectState::Intended {
        cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    }
    effect_turn(cx, rec, Vec::new()).await
}

/// Reads one file of the workspace for the turn journaled at `since`. A path that can never
/// name a file of it is refused here, journaled escaped, and never becomes an effect.
async fn read_file<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    turn: u32,
    base: Digest,
    path: String,
) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    let request = Digest::of(path.as_bytes());
    let after = journal::events_after(db, task, since)?;
    if let Some(id) = journal::intended(&after, "ReadFile", Some(&request))? {
        return effect_turn(cx, db.effect(&id)?, Vec::new()).await;
    }
    if let Some(denied) = journal::denial(&after) {
        return Ok(Next::Observe(journal::denial_observation(denied)?));
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    if let Err(detail) = check_path(&path) {
        let audit = json!({ "action": "ReadFile", "reason": "InvalidPath", "path": guest_text(&path), "detail": detail });
        db.append_audit(task, "Denied", &audit)?;
        tracing::info!(task_id = %task, %audit, "file read denied");
        return Ok(Next::Observe(Observation::FileReadRejected {
            reason: audit.to_string(),
        }));
    }
    let rec = match intend_read(
        db,
        task,
        EffectKind::ReadFile { path, turn },
        request,
        &base,
    )? {
        Ok(rec) => rec,
        Err(next) => return Ok(next),
    };
    if rec.state == EffectState::Intended {
        cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    }
    effect_turn(cx, rec, Vec::new()).await
}

/// How often the session's mailbox is looked at while nothing is waiting in it.
const MAILBOX_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The model request a session sends, as the runner forwards it: normalized (see
/// [`normalize_request`]) with the session's model in place of the CLI's, and the name the CLI
/// asked for. `Err` is why the request is refused.
fn session_body(
    body: &[u8],
    cap: u32,
    model: &str,
) -> std::result::Result<(Vec<u8>, String), String> {
    let normalized = normalize_request(body, cap)?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&normalized).map_err(|e| e.to_string())?;
    let requested: String = value["model"]
        .as_str()
        .unwrap_or("unknown")
        .chars()
        .take(128)
        .collect();
    value["model"] = json!(model);
    let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    Ok((bytes, requested))
}

/// The body of an error answer to a session's request, in the Messages API's error shape.
fn error_reply(kind: &str, message: &str) -> Vec<u8> {
    json!({ "type": "error", "error": { "type": kind, "message": guest_text(message) } })
        .to_string()
        .into_bytes()
}

/// The response bytes of the `ModelCall` for `request` journaled after `window`: the blob its
/// effect published.
fn response_bytes<E>(cx: &Cx<'_, E>, window: u64, request: &Digest) -> Result<Vec<u8>> {
    let events = journal::events_after(cx.db, &cx.task, window)?;
    let id = journal::intended(&events, "ModelCall", Some(request))?.ok_or_else(|| {
        EngineError::Protocol(format!("the model call {request} was not journaled"))
    })?;
    let rec = cx.db.effect(&id)?;
    let digest = rec
        .result_digest
        .ok_or_else(|| EngineError::Protocol(format!("model call {id} has no response")))?;
    Ok(cx.blobs.get(&digest)?)
}

/// Serves the model requests of the session whose job is `effect`, strictly one at a time and
/// in order, until a guard ends the task: the state it ends in. Each request is a journaled
/// `ModelCall` through [`call_model`], and its reply is the response, or the error its failure
/// maps to. Nothing is replied to a request that is refused before the call.
async fn serve_mailbox<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    session_turn: u32,
    base: Digest,
    effect: &EffectId,
    model: &str,
) -> Result<TaskState> {
    let (db, task) = (cx.db, &cx.task);
    let mailbox = loop {
        match cx.exec.session_mailbox(effect) {
            Some(mailbox) => break mailbox,
            None => tokio::time::sleep(MAILBOX_POLL).await,
        }
    };
    let cap = cx.contract.limits.max_output_tokens_per_request;
    let mut answered = 0u64;
    loop {
        let Some((id, body)) = mailbox.controller_next_request(answered)? else {
            tokio::time::sleep(MAILBOX_POLL).await;
            continue;
        };
        answered = id;
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        if let Some(state) = deadline_stop(cx).await? {
            return Ok(state);
        }
        if let Err(denial) = granted(db, task, Capability::ModelRequest, &Resource::Task)? {
            let capability = capability_name(Capability::ModelRequest);
            let audit = json!({ "action": "CallModel", "reason": "CapabilityDenied", "capability": capability, "denial": denial });
            db.append_audit(task, "Denied", &audit)?;
            tracing::info!(task_id = %task, %audit, "model call denied");
            return fail(db, task, &format!("capability {capability} not granted"));
        }
        if cx.model_limits_version == 1
            && let Err(reason) = check_request_size(body.len())
        {
            return fail(db, task, reason);
        }
        if cx.model_policy_version == 1
            && let Some(schedule) = pending_model_retry(cx)?
            && let Some(state) = wait_model_retry(cx, &schedule).await?
        {
            return Ok(state);
        }
        let (body, requested) = match session_body(&body, cap, model) {
            Ok(forwarded) => forwarded,
            Err(reason) => {
                // Refused before any call: no budget is spent on it.
                mailbox.controller_post_reply(
                    id,
                    400,
                    &error_reply("invalid_request_error", &reason),
                )?;
                continue;
            }
        };
        let request = Digest::of(&body);
        // The request's own call is looked for among the events journaled from here on.
        let window = db.events(task)?.last().map_or(since, |e| e.seq.max(since));
        journal::append_session_call(db, task, id, &request, &requested, model)?;
        ensure_request_artifact(cx, None, &request, &body)?;
        let turn = session_turn.saturating_add(u32::try_from(id).unwrap_or(u32::MAX));
        let obs = match interruptible(
            db,
            task,
            call_model(cx, window, turn, base, request, body).await,
        )? {
            Next::Stop(state) => return Ok(state),
            Next::Observe(obs) => obs,
        };
        match obs {
            Observation::ModelResponse { .. } => {
                let bytes = response_bytes(cx, window, &request)?;
                mailbox.controller_post_reply(id, 200, &bytes)?;
            }
            obs @ (Observation::ModelCallFailed { .. } | Observation::ModelCallLost) => {
                if let Some(state) = model_failure_policy(cx, &obs, turn.saturating_add(1)).await? {
                    return Ok(state);
                }
                let (status, message) = match &obs {
                    Observation::ModelCallFailed { reason, failure } => {
                        let transient = matches!(
                            failure,
                            Some(ModelFailure {
                                class: ModelFailureClass::Transient,
                                ..
                            })
                        );
                        (if transient { 529 } else { 400 }, reason.clone())
                    }
                    _ => (502, "the model call was lost".to_string()),
                };
                mailbox.controller_post_reply(id, status, &error_reply("api_error", &message))?;
            }
            other => {
                return Err(EngineError::Protocol(format!(
                    "a session's model call gave {other:?}"
                )));
            }
        }
    }
}

/// The observation of a session whose effect is decided: its end, or the task's failure when
/// the session failed (a session cut short by the task's deadline is the deadline's failure).
fn session_end<E: Executor>(cx: &Cx<'_, E>, rec: &EffectRecord) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    match rec.state {
        EffectState::Completed => Ok(Next::Observe(journal::effect_observation(cx.blobs, rec)?)),
        EffectState::Failed => {
            if db.deadline_passed(task)? {
                return Ok(Next::Stop(fail(db, task, recover::DEADLINE_EXCEEDED)?));
            }
            let reason = match rec.result_digest {
                Some(digest) => {
                    let result: serde_json::Value =
                        serde_json::from_slice(&cx.blobs.get(&digest)?).map_err(DbError::from)?;
                    result["reason"].as_str().unwrap_or("unknown").to_string()
                }
                None => "unknown".to_string(),
            };
            Ok(Next::Stop(fail(
                db,
                task,
                &format!("agent session failed: {reason}"),
            )?))
        }
        state => Err(EngineError::UnexpectedEffectState {
            effect: rec.effect_id.clone(),
            state,
        }),
    }
}

/// Runs the agent session of the turn journaled at `since`. The session effect runs as a job
/// while its model requests are served from the job's mailbox ([`serve_mailbox`]). The job's
/// end is the observation; a guard that stops the task ends the mailbox first, then the job is
/// cancelled and its attempt awaited, and the task ends as the guard said.
async fn run_session<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    turn: u32,
    base: Digest,
    spec: AgentSessionSpec,
    model: String,
) -> Result<Next> {
    let (db, task) = (cx.db, &cx.task);
    if let Err(denial) = granted(db, task, Capability::AgentSession, &Resource::Task)? {
        let capability = capability_name(Capability::AgentSession);
        let audit = json!({ "action": "RunSession", "reason": "CapabilityDenied", "capability": capability, "denial": denial });
        db.append_audit(task, "Denied", &audit)?;
        tracing::info!(task_id = %task, %audit, "agent session denied");
        return Ok(Next::Stop(fail(
            db,
            task,
            &format!("capability {capability} not granted"),
        )?));
    }
    let after = journal::events_after(db, task, since)?;
    if journal::intended(&after, "RunAgentSession", None)?.is_some() {
        // Recovery of an open session is not implemented: a session is never run twice.
        return Ok(Next::Stop(fail(db, task, "agent session lost")?));
    }
    if !cx.exec.runs_agent_sessions() {
        return Ok(Next::Stop(fail(
            db,
            task,
            "executor cannot run agent sessions",
        )?));
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Next::Stop(state));
    }
    let payload = spec.to_payload();
    let kind = EffectKind::RunAgentSession {
        expected_base: base,
    };
    let rec = match intend(db, task, kind, Digest::of(&payload), &base, &Resource::Task) {
        Ok(rec) => rec,
        Err(EngineError::Db(DbError::BudgetExceeded(_))) => {
            return Ok(Next::Stop(fail(db, task, "budget exhausted")?));
        }
        Err(EngineError::Db(DbError::CapabilityDenied { capability, .. })) => {
            let reason = format!("capability {} not granted", capability_name(capability));
            return Ok(Next::Stop(fail(db, task, &reason)?));
        }
        Err(e) => return Err(e),
    };
    if rec.state != EffectState::Intended {
        return session_end(cx, &rec);
    }
    cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;

    let mut attempt = std::pin::pin!(run_attempt(cx, &rec, payload));
    let mut serving = std::pin::pin!(serve_mailbox(cx, since, turn, base, &rec.effect_id, &model));
    let mut stop = None;
    let decided = loop {
        tokio::select! {
            done = &mut attempt => break done,
            served = &mut serving, if stop.is_none() => {
                cx.exec.cancel_jobs(std::slice::from_ref(&rec.effect_id));
                stop = Some(served);
            }
        }
    };
    if let Some(served) = stop {
        // The task already stopped: an attempt that failed alongside it (for instance one that
        // raced with recovery over the same effect) changes nothing the stop did not.
        if let Err(e) = &decided {
            tracing::warn!(task_id = %task, error = %e, "the session's attempt failed after the task stopped");
        }
        return Ok(Next::Stop(served?));
    }
    match decided? {
        Attempt::Published(ReceiptVerdict::Apply) => session_end(cx, &db.effect(&rec.effect_id)?),
        Attempt::Published(verdict) => Err(EngineError::ReceiptNotApplied {
            effect: rec.effect_id.clone(),
            verdict,
        }),
        Attempt::Ended(state) => Ok(Next::Stop(state)),
        Attempt::Forfeited => Err(EngineError::Protocol(
            "an agent session cannot be forfeited".into(),
        )),
    }
}

async fn act<E: Executor>(
    cx: &Cx<'_, E>,
    since: u64,
    turn: u32,
    base: Digest,
    action: AgentAction,
) -> Result<Next> {
    match action {
        AgentAction::ApplyPatch(patch) => apply_patch(cx, since, base, patch).await,
        AgentAction::Verify => verify(cx, Some(since)).await,
        AgentAction::Finish => Ok(Next::Stop(finish(cx.db, &cx.task)?)),
        AgentAction::CallModel { request, body } => {
            call_model(cx, since, turn, base, request, body).await
        }
        AgentAction::ListFiles => list_files(cx, since, turn, base).await,
        AgentAction::ReadFile(path) => read_file(cx, since, turn, base, path).await,
        AgentAction::RunSession { argv, env, model } => {
            let spec = AgentSessionSpec { argv, env };
            run_session(cx, since, turn, base, spec, model).await
        }
    }
}

/// The effect kind an action will intend, for the crash hook.
fn action_kind(action: &AgentAction) -> Option<&'static str> {
    match action {
        AgentAction::ApplyPatch(_) => Some("apply_patch"),
        AgentAction::Verify => Some("run_verification"),
        AgentAction::CallModel { .. } => Some("model_call"),
        AgentAction::ListFiles => Some("list_files"),
        AgentAction::ReadFile(_) => Some("read_file"),
        AgentAction::RunSession { .. } => Some("run_agent_session"),
        AgentAction::Finish => None,
    }
}

fn describe(action: &AgentAction) -> String {
    match action {
        AgentAction::ApplyPatch(patch) => format!("ApplyPatch({})", Digest::of(patch.as_bytes())),
        AgentAction::CallModel { request, .. } => format!("CallModel({request})"),
        // A session's argv, env and model are compared by digest: they are journaled once.
        AgentAction::RunSession { .. } => format!(
            "RunSession({})",
            Digest::of(&serde_json::to_vec(action).unwrap_or_default())
        ),
        other => format!("{other:?}"),
    }
}

fn observed_workspace(obs: &Observation) -> Option<Digest> {
    match obs {
        Observation::Start { workspace, .. } | Observation::PatchApplied { workspace } => {
            Some(*workspace)
        }
        Observation::VersionConflict { actual, .. } => Some(*actual),
        _ => None,
    }
}

fn pending_model_retry<E>(cx: &Cx<'_, E>) -> Result<Option<ModelRetrySchedule>> {
    let events = cx.db.events(&cx.task)?;
    let Some(index) = events
        .iter()
        .rposition(|e| e.event_type == "ModelRetryScheduled")
    else {
        return Ok(None);
    };
    if events[index + 1..]
        .iter()
        .any(|e| e.event_type == "EffectIntended" && e.payload["kind"].get("ModelCall").is_some())
    {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_value(events[index].payload.clone()).map_err(DbError::from)?,
    ))
}

async fn wait_model_retry<E: Executor>(
    cx: &Cx<'_, E>,
    schedule: &ModelRetrySchedule,
) -> Result<Option<TaskState>> {
    loop {
        if let Some(state) = interrupted(cx.db, &cx.task)? {
            return Ok(Some(state));
        }
        if let Some(state) = deadline_stop(cx).await? {
            return Ok(Some(state));
        }
        match cx
            .db
            .check(&cx.task, Capability::ModelRequest, &Resource::Task)
        {
            Ok(()) => {}
            Err(DbError::CapabilityDenied { .. }) => {
                return Ok(Some(fail(
                    cx.db,
                    &cx.task,
                    "capability model.request not granted during retry wait",
                )?));
            }
            Err(error) => return Err(error.into()),
        }
        let deadline = cx.db.deadline_ts(&cx.task)?;
        if deadline != 0 && schedule.not_before_ts >= deadline {
            return Ok(Some(fail(
                cx.db,
                &cx.task,
                "model retry would exceed task deadline",
            )?));
        }
        if cx.db.now() >= schedule.not_before_ts {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn model_failure_policy<E: Executor>(
    cx: &Cx<'_, E>,
    obs: &Observation,
    retry_turn: u32,
) -> Result<Option<TaskState>> {
    if cx.model_policy_version == 0 {
        return Ok(None);
    }
    let provider_not_before = match obs {
        Observation::ModelCallFailed { reason, failure } => {
            match failure
                .as_ref()
                .map(|f| f.class)
                .unwrap_or(ModelFailureClass::Permanent)
            {
                ModelFailureClass::Permanent => return Ok(Some(fail(cx.db, &cx.task, reason)?)),
                ModelFailureClass::Transient => {
                    failure.as_ref().and_then(|f| f.retry_not_before_ts)
                }
            }
        }
        Observation::ModelCallLost => None,
        _ => return Ok(None),
    };
    let events = cx.db.events(&cx.task)?;
    let mut failures = 0u32;
    let mut failed_effect = None;
    for event in events.iter().rev().filter(|e| {
        e.event_type == "EffectIntended" && e.payload["kind"].get("ModelCall").is_some()
    }) {
        let effect: EffectId =
            serde_json::from_value(event.payload["effect_id"].clone()).map_err(DbError::from)?;
        let rec = cx.db.effect(&effect)?;
        if rec.state == EffectState::Completed {
            break;
        }
        if rec.state == EffectState::Failed {
            if failed_effect.is_none() {
                failed_effect = Some(effect);
            }
            failures = failures.saturating_add(1);
        }
    }
    let effect = failed_effect
        .ok_or_else(|| EngineError::Protocol("model failure has no failed effect".into()))?;
    let schedule = cx.db.schedule_model_retry(
        &cx.task,
        &effect,
        retry_turn,
        backoff_seconds(failures),
        provider_not_before,
        cx.model_policy_version,
    )?;
    // The retry schedule is a durable completion boundary distinct from a model send.
    cx.crash(CrashPoint::AfterComplete, Some("model_retry"))?;
    wait_model_retry(cx, &schedule).await
}

/// Drives `task` until it is terminal, paused or waiting. Resumable: outstanding effects
/// of a crashed run are recovered first ([`crate::recover`]); the open session's journaled
/// turns are replayed into `agent` (which must be deterministic: a divergence fails the task
/// with `NondeterministicAgent`); the snapshot is not taken twice. A terminal task is
/// returned untouched; a PAUSED one is left for the caller to resume with `Resumed`.
pub async fn run_task<E: Executor, A: Agent>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    agent: &mut A,
    task: &TaskId,
) -> std::result::Result<TaskState, EngineError> {
    run_task_with(db, blobs, executor, agent, task, &RunOptions::default()).await
}

/// [`run_task`] with options (crash injection).
pub async fn run_task_with<E: Executor, A: Agent>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    agent: &mut A,
    task: &TaskId,
    opts: &RunOptions,
) -> std::result::Result<TaskState, EngineError> {
    let cx = Cx::new(db, blobs, executor, task, opts)?;
    let state = drive(&cx, agent).await?;
    if state.is_terminal() {
        // An intent left behind (e.g. a cancel landed between intent and dispatch) can
        // never run now; release its reservation.
        db.abandon_outstanding(task)?;
    }
    Ok(state)
}

/// Runs the contract's analyzer once over the snapshot, before the agent's first turn: a
/// no-op without an analyzer, or once the journal holds the analysis. The report is advisory:
/// a failed analysis, or a capability revoked before it, does not stop the task.
async fn ensure_analysis<E: Executor>(cx: &Cx<'_, E>) -> Result<Option<TaskState>> {
    let Some(analyzer) = &cx.contract.analyzer else {
        return Ok(None);
    };
    let (db, task) = (cx.db, &cx.task);
    if journal::intended(&db.events(task)?, "AnalyzeSnapshot", None)?.is_some() {
        // Recovery has already settled it if it was in flight.
        return Ok(None);
    }
    if let Some(state) = deadline_stop(cx).await? {
        return Ok(Some(state));
    }
    if let Err(denial) = granted(db, task, Capability::SnapshotAnalyze, &Resource::Task)? {
        tracing::info!(task_id = %task, denial, "analysis skipped: denied by the broker");
        db.append_audit(
            task,
            "AnalysisSkipped",
            &json!({ "reason": format!("capability {} not usable ({denial})",
                capability_name(Capability::SnapshotAnalyze)) }),
        )?;
        return Ok(None);
    }
    let t = db.task(task)?;
    let payload =
        crate::analysis::request_payload(&cx.contract, t.workspace_digest).ok_or_else(|| {
            EngineError::Protocol(format!(
                "analyzer digest {:?} is not a digest",
                analyzer.digest
            ))
        })?;
    let rec = intend(
        db,
        task,
        EffectKind::AnalyzeSnapshot,
        Digest::of(&payload),
        &t.workspace_digest,
        &Resource::Task,
    )?;
    cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
    match run_attempt(cx, &rec, payload).await? {
        Attempt::Published(ReceiptVerdict::Apply) | Attempt::Forfeited => Ok(None),
        Attempt::Published(verdict) => Err(EngineError::ReceiptNotApplied {
            effect: rec.effect_id,
            verdict,
        }),
        Attempt::Ended(state) => Ok(Some(state)),
    }
}

async fn drive<E: Executor, A: Agent>(cx: &Cx<'_, E>, agent: &mut A) -> Result<TaskState> {
    let (db, task) = (cx.db, &cx.task);
    // In-flight effects are reconciled before anything else, a pending cancel included,
    // so every effect is decided before the cancel is honoured. (The journal's workspace
    // digest can still lag the disk: a patch completing under a pending cancel has its
    // WorkspaceUpdated journaled as TaskEventRejected; its effect result holds the truth.)
    if !db.outstanding_effects(task)?.is_empty() {
        recover::reconcile(cx).await?;
    }
    if let Some(state) = interrupted(db, task)? {
        return Ok(state);
    }
    if db.task(task)?.state == TaskState::Ready {
        db.append(task, &TaskEvent::Started)?;
        tracing::info!(task_id = %task, "task started");
    }
    let (files, resumed) = match or_interrupted(db, task, ensure_snapshot(cx).await)? {
        Ok(Ok(found)) => found,
        Ok(Err(state)) | Err(state) => return Ok(state),
    };
    if resumed && let Some(state) = workspace_lost(cx)? {
        return Ok(state);
    }
    match or_interrupted(db, task, ensure_analysis(cx).await)? {
        Ok(Some(state)) | Err(state) => return Ok(state),
        Ok(None) => {}
    }

    let turns = journal::session_turns(&db.events(task)?)?;
    let t = db.task(task)?;
    let mut base = t.workspace_digest;
    // The last turn's *emitted* action is what is performed again below: unlike the journaled
    // one it carries a model request's body.
    let mut last_emitted = None;
    for turn in &turns {
        base = observed_workspace(&turn.observation).unwrap_or(base);
        let emitted = agent.next(&turn.observation);
        // Compared by description (a model request by its digest): the body is not journaled.
        if describe(&emitted) != describe(&turn.action) {
            fail(
                db,
                task,
                &format!("agent replay diverged at turn {}", turn.turn),
            )?;
            return Err(EngineError::NondeterministicAgent {
                turn: turn.turn,
                journaled: describe(&turn.action),
                emitted: describe(&emitted),
            });
        }
        last_emitted = Some(emitted);
    }
    let mut next = match turns.last().zip(last_emitted) {
        // The last journaled action may be unfinished: perform it, idempotently.
        Some((last, action)) => match interrupted(db, task)? {
            Some(state) => return Ok(state),
            None => interruptible(db, task, act(cx, last.seq, last.turn, base, action).await)?,
        },
        None if t.state == TaskState::Verifying => interruptible(db, task, verify(cx, None).await)?,
        None => Next::Observe(Observation::Start {
            files,
            workspace: base,
        }),
    };
    let mut turn = turns.len() as u32;
    loop {
        let mut obs = match next {
            Next::Observe(obs) => obs,
            Next::Stop(state) => return Ok(state),
        };
        // Legacy journals retain their original observation shape and decisions.
        if cx.model_policy_version == 0
            && let Observation::ModelCallFailed { failure, .. } = &mut obs
        {
            *failure = None;
        }
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        if let Some(state) = deadline_stop(cx).await? {
            return Ok(state);
        }
        if let Some(state) = model_failure_policy(cx, &obs, turn.saturating_add(1)).await? {
            return Ok(state);
        }
        if turn >= turn_limit(&cx.contract) {
            return fail(db, task, "agent turn limit exceeded");
        }
        turn += 1;
        base = observed_workspace(&obs).unwrap_or(base);
        let action = agent.next(&obs);
        // A pause or cancel that arrived while the agent was deciding wins over its action.
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        if let Some(state) = deadline_stop(cx).await? {
            return Ok(state);
        }
        // The request body is a registered artifact before the turn that names it exists, or
        // recovery's blob collection could delete it and an intended call would be lost.
        if let AgentAction::CallModel { request, body } = &action {
            if cx.model_limits_version == 1
                && let Err(reason) = check_request_size(body.len())
            {
                return fail(db, task, reason);
            }
            if cx.model_policy_version == 1
                && let Some(schedule) = pending_model_retry(cx)?
                && let Some(state) = wait_model_retry(cx, &schedule).await?
            {
                return Ok(state);
            }
            ensure_request_artifact(cx, None, request, body)?;
        }
        let since = journal::append_turn(db, task, turn, &obs, &action)?;
        cx.crash(CrashPoint::AfterAgentTurnJournaled, action_kind(&action))?;
        next = interruptible(db, task, act(cx, since, turn, base, action).await)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(request: Digest) -> EffectRecord {
        let kind = EffectKind::ModelCall {
            model: "m".into(),
            turn: 1,
        };
        EffectRecord {
            effect_id: EffectId::derive(&TaskId::new(), 1, &kind, &request),
            task_id: TaskId::new(),
            step: 1,
            kind,
            state: EffectState::Intended,
            request_digest: request,
            lease_generation: 0,
            result_digest: None,
        }
    }

    #[test]
    fn a_journaled_call_without_a_body_is_resent_from_its_stored_request() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let body = br#"{"model":"m","messages":[]}"#.to_vec();
        let request = Digest::of(&body);

        // A body that hashes to the request is used as it is.
        assert_eq!(
            request_body(&blobs, &request, body.clone(), None).unwrap(),
            body
        );
        // An empty one (what the journal holds) needs the blob; without it the call is lost.
        assert!(matches!(
            request_body(&blobs, &request, Vec::new(), None),
            Err(EngineError::Protocol(_))
        ));
        assert!(matches!(
            request_body(&blobs, &request, Vec::new(), Some(&rec(request))),
            Err(EngineError::Protocol(_))
        ));
        blobs.put(&body).unwrap();
        assert_eq!(
            request_body(&blobs, &request, Vec::new(), None).unwrap(),
            body
        );
        assert_eq!(
            request_body(&blobs, &request, Vec::new(), Some(&rec(request))).unwrap(),
            body
        );
        // A body for another request is not trusted either.
        assert_eq!(
            request_body(&blobs, &request, b"other".to_vec(), None).unwrap(),
            body
        );
    }

    #[test]
    fn the_model_name_comes_from_the_body_and_is_bounded() {
        assert_eq!(
            model_of(br#"{"model":"claude-opus-5-5"}"#),
            "claude-opus-5-5"
        );
        assert_eq!(model_of(b"not json"), "unknown");
        assert_eq!(model_of(br#"{"model":7}"#), "unknown");
        let long = format!(r#"{{"model":"{}"}}"#, "x".repeat(500));
        assert_eq!(model_of(long.as_bytes()).len(), 128);
    }
}
