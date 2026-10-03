//! Crash recovery: bring every outstanding effect of a task to a decided state before the
//! run loop resumes. On a task that can still dispatch, the journal, the usage ledger and
//! the workspace then agree again. Two exceptions: a patch that completes while a cancel is
//! pending has its `WorkspaceUpdated` journaled as `TaskEventRejected`, so the task's
//! workspace digest lags the disk (the effect's published result holds the true digest);
//! and an unreconcilable effect stays UNKNOWN, so whether it took effect is not known.
//!
//! Order of work:
//! 1. `BlobStore::gc` against the registered artifacts. It is only safe with no concurrent
//!    writers: recovery runs when the controller starts, before it dispatches anything (a
//!    live run of another task sharing the store would lose a blob it put but has not
//!    registered yet). Blobs left by a crash between put and register go away here.
//! 2. Each outstanding effect, in creation order, gets a decision, journaled as a
//!    `RecoveryDecision` audit event *before* it is acted on (see [`Decision`]). Receipts
//!    are job directories: every attempt ran as a supervised job, which may still be running
//!    (the controller's death does not stop it) and writes its receipt there.
//!    - a usable receipt retained by the executor is published and applied, never
//!      re-executed; a stale or malformed one goes through `complete_effect`'s verdicts
//!      (journaled as ignored) and is otherwise disregarded;
//!    - INTENDED effects are dispatched (lease 1) within their original reservation;
//!    - for DISPATCHED/UNKNOWN ones, recovery first makes sure no attempt still runs: it
//!      waits for the effect's live jobs, bounded by their lease plus a grace period, and
//!      re-reads the receipts (a job that finished while the controller was down is
//!      published like any retained receipt). It never kills a job inside its lease; one
//!      still alive past the bound is fenced (`WaitedForJob`), and one that cannot be
//!      stopped leaves the effect UNKNOWN and fails the task (`FenceFailed`).
//!    - with every attempt dead and no usable receipt, they follow their kind's retry
//!      policy: `Retry` re-dispatches under the next lease; `ReconcileThenRetry` asks the
//!      executor, which either proves the effect applied (its outcome is published),
//!      proves it not applied (re-dispatch) or cannot tell: the effect becomes UNKNOWN, its
//!      reservation stays `Uncertain`, and the task fails with "unreconcilable effect <id>".
//! 3. A pending cancel is honoured once nothing is in flight; a task that can never dispatch
//!    again (terminal, or cancel pending) dispatches nothing: never-dispatched effects and
//!    effects proven not applied are abandoned (reservation released), receipt-less ones
//!    of retryable kinds become UNKNOWN (they may have run).
//!
//! A paused or waiting task cannot dispatch now but may resume: effects that would need a
//! dispatch are deferred untouched (no decision is journaled) until it does.
//!
//! Recovery is idempotent: a decision is journaled only when it changes something, so
//! recovering an already recovered task journals nothing.

use std::time::Instant;

use agentos_core::effect::{accept_receipt, AttemptId, EffectId, EffectKind, EffectRecord, EffectState, ReceiptVerdict, RetryPolicy};
use agentos_core::ids::TaskId;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::crash::RunOptions;
use crate::executor::{AttemptCtx, ExecOutcome, Executor, JobWait, Reconciliation};
use crate::journal;
use crate::runner::{fail, recovered_patch, EngineError, Result};
use crate::steps::{check_outcome, finish_attempt, mark_unreconcilable, request, run_attempt, Attempt, Cx, WORKER};

/// What recovery did with one outstanding effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// INTENDED, never dispatched: dispatched now under its original reservation.
    Dispatch,
    /// The executor retained a receipt: published and applied without re-executing.
    PublishRetained,
    /// No receipt, but the executor proved the effect applied: its outcome is applied.
    PublishReconciled,
    /// No receipt and safe to run again: re-dispatched under the next lease generation.
    Redispatch,
    /// No receipt, may have run, and the task can no longer retry it: left UNKNOWN.
    MarkUnknown,
    /// No receipt and the executor cannot tell whether it took effect: left UNKNOWN and the
    /// task failed.
    Unreconcilable,
    /// No receipt and the kind must never be re-sent under the same reservation: FAILED,
    /// reservation `Uncertain`; the agent asks again.
    Forfeit,
    /// Provably never took effect and the task can no longer dispatch it: abandoned.
    Abandon,
    /// A job of the effect was still alive past its lease bound and is fenced; journaled
    /// before the fence acts. The decision journaled next acts on what the fence left
    /// (`FenceFailed` if it could not stop the job).
    WaitedForJob,
    /// A job of the effect could not be stopped: left UNKNOWN and the task failed.
    FenceFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDecision {
    pub effect_id: EffectId,
    pub kind: String,
    pub found_state: EffectState,
    pub lease_generation: u64,
    pub decision: Decision,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Files the blob-store collection removed.
    pub gc_removed: usize,
    pub decisions: Vec<RecoveryDecision>,
    /// Effects left as they are because the task is paused or waiting.
    pub deferred: Vec<EffectId>,
    /// Effects abandoned, by a decision or because the task ended with them intended.
    pub abandoned: Vec<EffectId>,
    /// The task's state afterwards.
    pub state: Option<TaskState>,
}

/// Recovers `task` after a controller restart. See the module docs.
pub async fn recover<E: Executor>(db: &Db, blobs: &BlobStore, executor: &E, task: &TaskId) -> Result<RecoveryReport> {
    recover_with(db, blobs, executor, task, &RunOptions::default()).await
}

/// [`recover`] with options (crash injection: recovery itself can be killed).
pub async fn recover_with<E: Executor>(
    db: &Db,
    blobs: &BlobStore,
    executor: &E,
    task: &TaskId,
    opts: &RunOptions,
) -> Result<RecoveryReport> {
    reconcile(&Cx::new(db, blobs, executor, task, opts)?).await
}

pub const DEADLINE_EXCEEDED: &str = "deadline exceeded";

/// Whether `task`'s deadline has passed while it could still run: a terminal task has
/// nothing to enforce, and a pending cancel wins over the deadline (it ends CANCELLED).
fn deadline_due<E>(cx: &Cx<'_, E>) -> Result<bool> {
    if cx.closing.is_some() {
        return Ok(false);
    }
    let t = cx.db.task(&cx.task)?;
    Ok(!t.state.is_terminal() && !t.cancel_requested && cx.db.deadline_passed(&cx.task)?)
}

/// The deadline gate, consulted before each agent turn, intent and dispatch. Past the
/// deadline it runs recovery in closing mode (live jobs are waited for or fenced, their
/// receipts published, patches reconciled, nothing dispatched) and fails the task with
/// `deadline exceeded`; returns the task's state then. `None` while the task may go on.
pub(crate) async fn deadline_stop<E: Executor>(cx: &Cx<'_, E>) -> Result<Option<TaskState>> {
    if !deadline_due(cx)? {
        return Ok(None);
    }
    Ok(Some(close(cx, DEADLINE_EXCEEDED).await?))
}

/// Ends the task with `reason` through recovery's closing mode: what is in flight is
/// decided first (jobs waited for or fenced, receipts published, patches reconciled), then
/// the task fails, then whatever can only be abandoned on a failed task is. A pending cancel
/// still wins (the task ends CANCELLED). Returns the task's state.
pub(crate) async fn close<E: Executor>(cx: &Cx<'_, E>, reason: &str) -> Result<TaskState> {
    let closing = cx.closing_with(reason);
    // Boxed: closing recovery reaches `run_attempt` (which can close) again.
    let pass: std::pin::Pin<Box<dyn std::future::Future<Output = Result<RecoveryReport>> + '_>> = Box::pin(reconcile(&closing));
    Ok(pass.await?.state.unwrap_or(TaskState::Failed))
}

pub(crate) async fn reconcile<E: Executor>(cx: &Cx<'_, E>) -> Result<RecoveryReport> {
    let derived;
    let cx = if deadline_due(cx)? {
        tracing::info!(task_id = %cx.task, "deadline passed; closing the task");
        derived = cx.closing_with(DEADLINE_EXCEEDED);
        &derived
    } else {
        cx
    };
    let (db, task) = (cx.db, &cx.task);
    // Not in closing mode's mid-run passes: a blob the runner put but has not registered yet
    // would be lost. The next recovery collects what this skips.
    let gc_removed = if cx.closing.is_some() { 0 } else { cx.blobs.gc(&db.referenced_blobs()?)? };
    let mut report = RecoveryReport { gc_removed, ..RecoveryReport::default() };
    for rec in db.outstanding_effects(task)? {
        // A dispatch refused on the way can close the task and decide the later effects.
        let Some(rec) = db.outstanding_effects(task)?.into_iter().find(|e| e.effect_id == rec.effect_id) else { continue };
        recover_effect(cx, rec, &mut report).await?;
    }

    if let Some(reason) = &cx.closing {
        let t = db.task(task)?;
        if !t.state.is_terminal() && !t.cancel_requested {
            // Everything in flight is decided; now the task ends. Effects that can only be
            // abandoned on a task that cannot dispatch again (see `abandon`) go second.
            fail(db, task, reason)?;
            for rec in db.outstanding_effects(task)? {
                recover_effect(cx, rec, &mut report).await?;
            }
        }
    }
    let t = db.task(task)?;
    let in_flight = db.outstanding_effects(task)?.iter().any(|e| e.state == EffectState::Dispatched);
    if t.cancel_requested && !t.state.is_terminal() && !in_flight {
        tracing::info!(task_id = %task, "cancel completed after recovery");
        db.append(task, &TaskEvent::CancelCompleted)?;
    }
    let t = db.task(task)?;
    if t.state.is_terminal() || t.cancel_requested || cx.closing.is_some() {
        report.abandoned.extend(db.abandon_outstanding(task)?);
    }
    report.state = Some(db.task(task)?.state);
    tracing::info!(task_id = %task, ?report, "recovery finished");
    Ok(report)
}

const RETAINED: &str = "the executor retained a receipt for this effect";

fn decide<E>(cx: &Cx<'_, E>, report: &mut RecoveryReport, rec: &EffectRecord, decision: Decision, reason: &str) -> Result<()> {
    let d = RecoveryDecision {
        effect_id: rec.effect_id.clone(),
        kind: rec.kind.tag().to_string(),
        found_state: rec.state,
        lease_generation: rec.lease_generation,
        decision,
        reason: reason.to_string(),
    };
    cx.db.append_audit(&cx.task, "RecoveryDecision", &serde_json::to_value(&d).map_err(agentos_store::db::DbError::from)?)?;
    tracing::info!(task_id = %cx.task, effect_id = %rec.effect_id, ?decision, reason, "recovery decision");
    report.decisions.push(d);
    Ok(())
}

fn expect_applied(rec: &EffectRecord, verdict: ReceiptVerdict) -> Result<()> {
    if verdict != ReceiptVerdict::Apply {
        return Err(EngineError::ReceiptNotApplied { effect: rec.effect_id.clone(), verdict });
    }
    Ok(())
}

/// A new attempt either published (and its receipt must apply) or ended unresolved, in
/// which case the effect is already UNKNOWN and the task failed.
fn expect_attempt(rec: &EffectRecord, attempt: Attempt) -> Result<()> {
    match attempt {
        Attempt::Published(verdict) => expect_applied(rec, verdict),
        Attempt::Ended(_) | Attempt::Forfeited => Ok(()),
    }
}

/// The request payload of `rec`: the patch text for ApplyPatch, empty otherwise. `None`
/// when the patch text is gone (no journaled turn and no blob holds it).
fn payload<E>(cx: &Cx<'_, E>, rec: &EffectRecord) -> Result<Option<Vec<u8>>> {
    match rec.kind {
        EffectKind::ApplyPatch { .. } => Ok(recovered_patch(cx, rec)?.map(String::into_bytes)),
        EffectKind::ModelCall { .. } => journal::model_request_body(cx.blobs, rec),
        _ => Ok(Some(Vec::new())),
    }
}

/// Uses a receipt the executor retained, if it can be applied. Returns whether it was.
/// `reason` is the journaled reason if it is published.
async fn use_retained<E: Executor>(
    cx: &Cx<'_, E>,
    rec: &EffectRecord,
    report: &mut RecoveryReport,
    reason: &str,
) -> Result<bool> {
    let Some(out) = cx.exec.retained_outcome(&rec.effect_id) else {
        return Ok(false);
    };
    let verdict = accept_receipt(rec, &out.receipt);
    if verdict == ReceiptVerdict::Apply && check_outcome(rec, &out).is_ok() {
        decide(cx, report, rec, Decision::PublishRetained, reason)?;
        expect_applied(rec, finish_attempt(cx, rec, &out)?)?;
        return Ok(true);
    }
    let attempt = serde_json::to_value(&out.receipt.attempt_id).map_err(agentos_store::db::DbError::from)?;
    if !journal::receipt_audited(cx.db, &cx.task, &attempt)? {
        if verdict == ReceiptVerdict::Apply {
            // Applicable by lease, but it does not describe its own output.
            let audit = json!({ "reason": "ResultDigestMismatch", "effect_id": rec.effect_id, "receipt": out.receipt });
            cx.db.append_audit(&cx.task, "RetainedReceiptRejected", &audit)?;
        } else {
            // The store re-derives the same verdict and journals the receipt as ignored.
            cx.db.complete_effect(&rec.effect_id, &out.receipt, None, None)?;
        }
    }
    Ok(false)
}

async fn recover_effect<E: Executor>(cx: &Cx<'_, E>, rec: EffectRecord, report: &mut RecoveryReport) -> Result<()> {
    let t = cx.db.task(&cx.task)?;
    let can_dispatch = t.may_dispatch() && cx.closing.is_none();
    // Terminal, cancel pending (cancel always wins), or being closed: this task never
    // dispatches again.
    let closing = t.state.is_terminal() || t.cancel_requested || cx.closing.is_some();

    if rec.state != EffectState::Intended && use_retained(cx, &rec, report, RETAINED).await? {
        return Ok(());
    }
    if rec.state == EffectState::Intended {
        if can_dispatch {
            let Some(payload) = payload(cx, &rec)? else {
                return unreconcilable(cx, report, &rec, "its request payload is no longer available");
            };
            decide(cx, report, &rec, Decision::Dispatch, "intended, never dispatched")?;
            return expect_attempt(&rec, run_attempt(cx, &rec, payload).await?);
        }
        if closing {
            return abandon(cx, report, &rec, "never dispatched, and the task can no longer dispatch it");
        }
        report.deferred.push(rec.effect_id);
        return Ok(());
    }
    if !can_dispatch && !closing {
        report.deferred.push(rec.effect_id);
        return Ok(());
    }

    // Dispatched: whatever is decided next, no earlier attempt may still be running.
    let started = Instant::now();
    if let JobWait::StillAlive = cx.exec.await_job(&rec.effect_id).await {
        // Journaled before the fence acts. An effect already left UNKNOWN on a terminal task
        // was decided before; fencing it again journals nothing.
        if !(t.state.is_terminal() && rec.state == EffectState::Unknown) {
            let reason = "a job of the effect is alive past its lease bound; fencing it";
            decide(cx, report, &rec, Decision::WaitedForJob, reason)?;
        }
        if !cx.exec.fence_job(&rec.effect_id).await {
            return fence_failed(cx, report, &rec);
        }
    }
    // Every attempt is dead now. There was no usable receipt before the wait, so one found
    // now came from a job that finished while we waited (or was left by the fenced one).
    let waited = started.elapsed();
    let reason = match waited.as_millis() {
        0 => RETAINED.to_string(),
        _ => format!("published after waiting {:.1} s for a live job", waited.as_secs_f64()),
    };
    if use_retained(cx, &rec, report, &reason).await? {
        return Ok(());
    }

    match rec.kind.retry_policy() {
        RetryPolicy::Retry if can_dispatch => {
            let Some(payload) = payload(cx, &rec)? else {
                return unreconcilable(cx, report, &rec, "its request payload is no longer available");
            };
            decide(cx, report, &rec, Decision::Redispatch, "no receipt; retry policy Retry: safe to run again")?;
            expect_attempt(&rec, run_attempt(cx, &rec, payload).await?)
        }
        RetryPolicy::Retry => {
            if rec.state == EffectState::Dispatched {
                let reason = "no receipt and the task can no longer retry it; it may have run";
                decide(cx, report, &rec, Decision::MarkUnknown, reason)?;
                cx.db.mark_unknown(&rec.effect_id)?;
            }
            Ok(())
        }
        RetryPolicy::ReconcileThenRetry => reconcile_effect(cx, rec, report, can_dispatch).await,
        RetryPolicy::NoRetry => unreconcilable(cx, report, &rec, "no receipt; retry policy NoRetry"),
        RetryPolicy::ForfeitThenRetry => forfeit(
            cx,
            report,
            &rec,
            "no receipt; retry policy ForfeitThenRetry: the request may have been sent and billed, so it is forfeited and asked again under a new reservation",
        ),
    }
}

async fn reconcile_effect<E: Executor>(
    cx: &Cx<'_, E>,
    rec: EffectRecord,
    report: &mut RecoveryReport,
    can_dispatch: bool,
) -> Result<()> {
    let Some(payload) = payload(cx, &rec)? else {
        return unreconcilable(cx, report, &rec, "its request payload is no longer available");
    };
    let ctx = AttemptCtx { attempt_id: AttemptId::new(), lease_generation: rec.lease_generation, worker: WORKER.into() };
    let req = request(&rec, payload.clone(), &cx.contract, cx.db.deadline_ts(&cx.task)?);
    match cx.exec.reconcile(&req, &ctx).await {
        Reconciliation::Applied(out) if usable(&rec, &out) => {
            let reason = "no receipt; reconciliation found the effect applied";
            decide(cx, report, &rec, Decision::PublishReconciled, reason)?;
            expect_applied(&rec, finish_attempt(cx, &rec, &out)?)
        }
        Reconciliation::Applied(_) => unreconcilable(cx, report, &rec, "reconciliation returned an unusable outcome"),
        Reconciliation::NotApplied if can_dispatch => {
            let reason = "no receipt; reconciliation found it not applied, so retry policy ReconcileThenRetry retries it";
            decide(cx, report, &rec, Decision::Redispatch, reason)?;
            expect_attempt(&rec, run_attempt(cx, &rec, payload).await?)
        }
        Reconciliation::NotApplied => {
            abandon(cx, report, &rec, "reconciliation found it not applied, and the task can no longer dispatch it")
        }
        Reconciliation::Unknown => unreconcilable(
            cx,
            report,
            &rec,
            "no receipt; retry policy ReconcileThenRetry, but the executor cannot tell whether it took effect",
        ),
    }
}

fn usable(rec: &EffectRecord, out: &ExecOutcome) -> bool {
    check_outcome(rec, out).is_ok() && accept_receipt(rec, &out.receipt) == ReceiptVerdict::Apply
}

fn abandon<E>(cx: &Cx<'_, E>, report: &mut RecoveryReport, rec: &EffectRecord, reason: &str) -> Result<()> {
    if cx.closing.is_some() {
        let t = cx.db.task(&cx.task)?;
        if !t.state.is_terminal() && !t.cancel_requested {
            // The store abandons only for a task that is terminal or being cancelled; this
            // one is failed at the end of the closing pass, and the effect is decided then.
            return Ok(());
        }
    }
    decide(cx, report, rec, Decision::Abandon, reason)?;
    cx.db.abandon_effect(&rec.effect_id, reason)?;
    report.abandoned.push(rec.effect_id.clone());
    Ok(())
}

/// FAILED without a result, the reservation `Uncertain`; the task goes on and the agent asks
/// again. A no-op once the effect is no longer DISPATCHED/UNKNOWN (decided before).
fn forfeit<E>(cx: &Cx<'_, E>, report: &mut RecoveryReport, rec: &EffectRecord, reason: &str) -> Result<()> {
    if !matches!(cx.db.effect(&rec.effect_id)?.state, EffectState::Dispatched | EffectState::Unknown) {
        return Ok(());
    }
    decide(cx, report, rec, Decision::Forfeit, reason)?;
    cx.db.forfeit_effect(&rec.effect_id, reason)?;
    Ok(())
}

/// A job of the effect survived the fence, so it may still take effect: the effect is left
/// UNKNOWN and the task failed. A no-op for an effect already UNKNOWN on a terminal task.
fn fence_failed<E>(cx: &Cx<'_, E>, report: &mut RecoveryReport, rec: &EffectRecord) -> Result<()> {
    if cx.db.task(&cx.task)?.state.is_terminal() && rec.state == EffectState::Unknown {
        return Ok(());
    }
    decide(cx, report, rec, Decision::FenceFailed, "a job of the effect is still alive and could not be stopped")?;
    mark_unreconcilable(cx.db, rec)?;
    Ok(())
}

/// Leaves the effect UNKNOWN (its reservation `Uncertain`) and fails the task. A no-op for
/// an effect already UNKNOWN on a terminal task: that decision was taken before.
fn unreconcilable<E>(cx: &Cx<'_, E>, report: &mut RecoveryReport, rec: &EffectRecord, reason: &str) -> Result<()> {
    let terminal = cx.db.task(&cx.task)?.state.is_terminal();
    if terminal && matches!(rec.state, EffectState::Unknown | EffectState::Intended) {
        return Ok(());
    }
    decide(cx, report, rec, Decision::Unreconcilable, reason)?;
    mark_unreconcilable(cx.db, rec)?;
    Ok(())
}
