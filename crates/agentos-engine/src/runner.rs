//! The run loop. The agent chooses an action, the runner journals it as an `AgentTurn`, then
//! performs it through the effect steps in [`crate::steps`]. Every action is idempotent
//! against the journal (see [`crate::journal`]), so after a crash `run_task` recovers the
//! outstanding effects ([`crate::recover`]), replays the open session into a fresh agent
//! and carries on exactly where the killed controller stopped.

use agentos_core::broker::Resource;
use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{EffectId, EffectKind, EffectRecord, EffectState, ReceiptVerdict};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState, TransitionError};
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError};
use serde_json::json;

use crate::agent::{Agent, AgentAction, Observation};
use crate::crash::{CrashPoint, RunOptions};
use crate::executor::Executor;
use crate::journal;
use crate::patch::patch_paths;
use crate::recover::{self, deadline_stop};
use crate::steps::{intend, run_attempt, Attempt, Cx};
use crate::workspace::has_excluded_component;

pub use crate::steps::{follow_up_event, verification_verdict, WORKER};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("blob store: {0}")]
    Blob(#[from] std::io::Error),
    #[error("effect {effect} is {state:?}, expected it to be freshly intended or finished")]
    UnexpectedEffectState { effect: EffectId, state: EffectState },
    #[error("receipt for effect {effect} was not applied: {verdict:?}")]
    ReceiptNotApplied { effect: EffectId, verdict: ReceiptVerdict },
    #[error("executor protocol violation: {0}")]
    Protocol(String),
    #[error("injected crash at {0}")]
    Crashed(CrashPoint),
    #[error("agent replay diverged at turn {turn}: journaled {journaled}, agent chose {emitted}")]
    NondeterministicAgent { turn: u32, journaled: String, emitted: String },
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
    contract.limits.tool_actions.saturating_mul(4).saturating_add(8)
}

/// The capability's contract name, e.g. `verification.run`.
fn capability_name(cap: Capability) -> String {
    serde_json::to_value(cap).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

pub(crate) fn fail(db: &Db, task: &TaskId, reason: &str) -> Result<TaskState> {
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

/// A pause or cancel can land between the runner's [`interrupted`] check and its next write;
/// the write is then refused (not dispatchable, or the reducer rejects `VerifyStarted`).
/// Such a refusal stops the run as the interrupt says; any other error propagates. Nothing
/// is lost: an intent left behind is dispatched on resume or abandoned once cancelled.
fn or_interrupted<T>(db: &Db, task: &TaskId, r: Result<T>) -> Result<std::result::Result<T, TaskState>> {
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
async fn effect_turn<E: Executor>(cx: &Cx<'_, E>, rec: EffectRecord, payload: Vec<u8>) -> Result<Next> {
    let rec = match rec.state {
        EffectState::Intended => match run_attempt(cx, &rec, payload).await? {
            Attempt::Published(ReceiptVerdict::Apply) => cx.db.effect(&rec.effect_id)?,
            Attempt::Published(verdict) => return Err(EngineError::ReceiptNotApplied { effect: rec.effect_id, verdict }),
            Attempt::Ended(state) => return Ok(Next::Stop(state)),
        },
        EffectState::Completed | EffectState::Failed => rec,
        state => return Err(EngineError::UnexpectedEffectState { effect: rec.effect_id, state }),
    };
    Ok(Next::Observe(journal::effect_observation(cx.blobs, &rec)?))
}

/// Ensures the workspace exists; returns the snapshot's file list and whether the snapshot
/// predates this call, or the state to stop in.
async fn ensure_snapshot<E: Executor>(cx: &Cx<'_, E>) -> Result<std::result::Result<(Vec<String>, bool), TaskState>> {
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
                let reason = format!("capability {} not granted", capability_name(Capability::SnapshotRead));
                return Ok(Err(fail(db, task, &reason)?));
            }
            let t = db.task(task)?;
            let request = Digest::of(cx.contract.repository.revision.as_bytes());
            let rec = intend(db, task, EffectKind::ReadSnapshot, request, &t.workspace_digest, &Resource::Task)?;
            cx.crash(CrashPoint::AfterIntent, Some(rec.kind.tag()))?;
            match run_attempt(cx, &rec, Vec::new()).await? {
                Attempt::Published(ReceiptVerdict::Apply) => (db.effect(&rec.effect_id)?, false),
                Attempt::Published(verdict) => return Err(EngineError::ReceiptNotApplied { effect: rec.effect_id, verdict }),
                Attempt::Ended(state) => return Ok(Err(state)),
            }
        }
    };
    if rec.state != EffectState::Completed {
        return Ok(Err(fail(db, task, "snapshot failed")?));
    }
    let digest = rec.result_digest.ok_or_else(|| EngineError::Protocol("snapshot without manifest".into()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&cx.blobs.get(&digest)?).map_err(DbError::from)?;
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
    cx.db.register_artifact(&rec.request_digest, patch.len() as u64, "patch", Some(&rec.effect_id), &provenance)?;
    Ok(())
}

/// Recovery's view of [`ensure_patch_artifact`]: the patch text for an ApplyPatch effect,
/// from the agent turn that chose it (or its blob), published again if it was collected.
pub(crate) fn recovered_patch<E>(cx: &Cx<'_, E>, rec: &EffectRecord) -> Result<Option<String>> {
    let text = match journal::journaled_patch(cx.db, &cx.task, &rec.request_digest)? {
        Some(text) => text,
        None if cx.blobs.exists(&rec.request_digest) => {
            String::from_utf8(cx.blobs.get(&rec.request_digest)?).map_err(|e| EngineError::Protocol(e.to_string()))?
        }
        None => return Ok(None),
    };
    ensure_patch_artifact(cx, rec, &text)?;
    Ok(Some(text))
}

/// The broker's pure pre-check (nothing journaled): `Err(reason)` when `op` on `resource`
/// would be denied. Any other store error propagates.
fn granted(db: &Db, task: &TaskId, op: Capability, resource: &Resource) -> Result<std::result::Result<(), String>> {
    match db.check(task, op, resource) {
        Ok(()) => Ok(Ok(())),
        Err(DbError::CapabilityDenied { reason, .. }) => Ok(Err(reason)),
        Err(e) => Err(e.into()),
    }
}

/// The broker pre-check: refused patches create no effect and consume no tool action.
/// Returns the patch's paths (the resource its intent is authorized on), or the denial.
async fn patch_denial(contract: &Contract, patch: &str, request: Digest) -> std::result::Result<Vec<String>, serde_json::Value> {
    let paths = patch_paths(patch).await.map_err(|detail| {
        json!({ "action": "ApplyPatch", "reason": "InvalidPatch", "detail": detail, "request_digest": request })
    })?;
    let refused = |reason: &str, bad: Vec<&String>| {
        (!bad.is_empty()).then(|| json!({ "action": "ApplyPatch", "reason": reason, "paths": bad, "request_digest": request }))
    };
    let denial = refused("PathNotEditable", paths.iter().filter(|p| !contract.path_allowed(p)).collect())
        .or_else(|| refused("DigestExcludedPath", paths.iter().filter(|p| has_excluded_component(p)).collect()));
    match denial {
        Some(audit) => Err(audit),
        None => Ok(paths),
    }
}

/// Applies `patch` for the turn journaled at `since`. Idempotent: an effect or denial the
/// turn already produced is reused (see [`crate::journal`]).
async fn apply_patch<E: Executor>(cx: &Cx<'_, E>, since: u64, base: Digest, patch: String) -> Result<Next> {
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
            return Ok(Next::Observe(Observation::PatchRejected { reason: audit.to_string() }));
        }
    };

    // Publish the patch itself first so the intent's request digest names a stored blob.
    cx.blobs.put(patch.as_bytes())?;
    let kind = EffectKind::ApplyPatch { expected_base: base };
    let rec = match intend(db, task, kind, request, &base, &Resource::Paths(paths)) {
        Ok(rec) => rec,
        Err(EngineError::Db(DbError::VersionConflict { expected, actual })) => {
            return Ok(Next::Observe(Observation::VersionConflict { expected, actual }));
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
        return Ok(Next::Observe(Observation::PatchRejected { reason: "this patch already failed to apply".into() }));
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
            let audit = json!({ "action": "Verify", "reason": "CapabilityDenied", "capability": capability });
            db.append_audit(task, "Denied", &audit)?;
            tracing::info!(task_id = %task, %audit, "verification denied");
        }
        let summary = format!("capability {capability} not granted");
        return Ok(Next::Observe(Observation::Verification { passed: false, summary }));
    }
    let t = db.task(task)?;
    let t = if t.state == TaskState::Verifying { t } else { db.append(task, &TaskEvent::VerifyStarted)? };
    tracing::info!(task_id = %task, step = t.step, "verification started");
    let workspace = t.workspace_digest;
    let rec = intend(db, task, EffectKind::RunVerification, Digest::of(workspace.as_bytes()), &workspace, &profile)?;
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

async fn act<E: Executor>(cx: &Cx<'_, E>, since: u64, base: Digest, action: AgentAction) -> Result<Next> {
    match action {
        AgentAction::ApplyPatch(patch) => apply_patch(cx, since, base, patch).await,
        AgentAction::Verify => verify(cx, Some(since)).await,
        AgentAction::Finish => Ok(Next::Stop(finish(cx.db, &cx.task)?)),
    }
}

/// The effect kind an action will intend, for the crash hook.
fn action_kind(action: &AgentAction) -> Option<&'static str> {
    match action {
        AgentAction::ApplyPatch(_) => Some("apply_patch"),
        AgentAction::Verify => Some("run_verification"),
        AgentAction::Finish => None,
    }
}

fn describe(action: &AgentAction) -> String {
    match action {
        AgentAction::ApplyPatch(patch) => format!("ApplyPatch({})", Digest::of(patch.as_bytes())),
        other => format!("{other:?}"),
    }
}

fn observed_workspace(obs: &Observation) -> Option<Digest> {
    match obs {
        Observation::Start { workspace, .. } | Observation::PatchApplied { workspace } => Some(*workspace),
        Observation::VersionConflict { actual, .. } => Some(*actual),
        _ => None,
    }
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
    if resumed {
        if let Some(state) = workspace_lost(cx)? {
            return Ok(state);
        }
    }

    let turns = journal::session_turns(&db.events(task)?)?;
    let t = db.task(task)?;
    let mut base = t.workspace_digest;
    for turn in &turns {
        base = observed_workspace(&turn.observation).unwrap_or(base);
        let emitted = agent.next(&turn.observation);
        if emitted != turn.action {
            fail(db, task, &format!("agent replay diverged at turn {}", turn.turn))?;
            return Err(EngineError::NondeterministicAgent {
                turn: turn.turn,
                journaled: describe(&turn.action),
                emitted: describe(&emitted),
            });
        }
    }
    let mut next = match turns.last() {
        // The last journaled action may be unfinished: perform it, idempotently.
        Some(last) => match interrupted(db, task)? {
            Some(state) => return Ok(state),
            None => interruptible(db, task, act(cx, last.seq, base, last.action.clone()).await)?,
        },
        None if t.state == TaskState::Verifying => interruptible(db, task, verify(cx, None).await)?,
        None => Next::Observe(Observation::Start { files, workspace: base }),
    };
    let mut turn = turns.len() as u32;
    loop {
        let obs = match next {
            Next::Observe(obs) => obs,
            Next::Stop(state) => return Ok(state),
        };
        if let Some(state) = interrupted(db, task)? {
            return Ok(state);
        }
        if let Some(state) = deadline_stop(cx).await? {
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
        let since = journal::append_turn(db, task, turn, &obs, &action)?;
        cx.crash(CrashPoint::AfterAgentTurnJournaled, action_kind(&action))?;
        next = interruptible(db, task, act(cx, since, base, action).await)?;
    }
}
