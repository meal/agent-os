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
//!    `RecoveryDecision` audit event *before* it is acted on (see [`Decision`]):
//!    - a usable receipt retained by the executor is published and applied, never
//!      re-executed; a stale or malformed one goes through `complete_effect`'s verdicts
//!      (journaled as ignored) and is otherwise disregarded;
//!    - INTENDED effects are dispatched (lease 1) within their original reservation;
//!    - DISPATCHED/UNKNOWN ones follow their kind's retry policy: `Retry` re-dispatches
//!      under the next lease; `ReconcileThenRetry` asks the executor, which either proves
//!      the effect applied (its outcome is published), proves it not applied (re-dispatch)
//!      or cannot tell: the effect becomes UNKNOWN, its reservation stays `Uncertain`, and
//!      the task fails with "unreconcilable effect <id>".
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

use agentos_core::effect::{accept_receipt, AttemptId, EffectId, EffectKind, EffectRecord, EffectState, ReceiptVerdict, RetryPolicy};
use agentos_core::ids::TaskId;
use agentos_core::state::{TaskEvent, TaskState};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::crash::RunOptions;
use crate::executor::{AttemptCtx, ExecOutcome, Executor, Reconciliation};
use crate::journal;
use crate::runner::{fail, recovered_patch, EngineError, Result};
use crate::steps::{check_outcome, finish_attempt, request, run_attempt, Cx, WORKER};

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
    /// Provably never took effect and the task can no longer dispatch it: abandoned.
    Abandon,
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

pub(crate) async fn reconcile<E: Executor>(cx: &Cx<'_, E>) -> Result<RecoveryReport> {
    let (db, task) = (cx.db, &cx.task);
    let gc_removed = cx.blobs.gc(&db.referenced_blobs()?)?;
    let mut report = RecoveryReport { gc_removed, ..RecoveryReport::default() };
    for rec in db.outstanding_effects(task)? {
        recover_effect(cx, rec, &mut report).await?;
    }

    let t = db.task(task)?;
    let in_flight = db.outstanding_effects(task)?.iter().any(|e| e.state == EffectState::Dispatched);
    if t.cancel_requested && !t.state.is_terminal() && !in_flight {
        tracing::info!(task_id = %task, "cancel completed after recovery");
        db.append(task, &TaskEvent::CancelCompleted)?;
    }
    let t = db.task(task)?;
    if t.state.is_terminal() || t.cancel_requested {
        report.abandoned.extend(db.abandon_outstanding(task)?);
    }
    report.state = Some(db.task(task)?.state);
    tracing::info!(task_id = %task, ?report, "recovery finished");
    Ok(report)
}

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

/// The request payload of `rec`: the patch text for ApplyPatch, empty otherwise. `None`
/// when the patch text is gone (no journaled turn and no blob holds it).
fn payload<E>(cx: &Cx<'_, E>, rec: &EffectRecord) -> Result<Option<Vec<u8>>> {
    match rec.kind {
        EffectKind::ApplyPatch { .. } => Ok(recovered_patch(cx, rec)?.map(String::into_bytes)),
        _ => Ok(Some(Vec::new())),
    }
}

/// Uses a receipt the executor retained, if it can be applied. Returns whether it was.
async fn use_retained<E: Executor>(cx: &Cx<'_, E>, rec: &EffectRecord, report: &mut RecoveryReport) -> Result<bool> {
    let Some(out) = cx.exec.retained_outcome(&rec.effect_id) else {
        return Ok(false);
    };
    let verdict = accept_receipt(rec, &out.receipt);
    if verdict == ReceiptVerdict::Apply && check_outcome(rec, &out).is_ok() {
        decide(cx, report, rec, Decision::PublishRetained, "the executor retained a receipt for this effect")?;
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
    let can_dispatch = t.may_dispatch();
    // Terminal, or cancel pending (cancel always wins): this task never dispatches again.
    let closing = t.state.is_terminal() || t.cancel_requested;

    if rec.state != EffectState::Intended && use_retained(cx, &rec, report).await? {
        return Ok(());
    }
    if rec.state == EffectState::Intended {
        if can_dispatch {
            let Some(payload) = payload(cx, &rec)? else {
                return unreconcilable(cx, report, &rec, "its request payload is no longer available");
            };
            decide(cx, report, &rec, Decision::Dispatch, "intended, never dispatched")?;
            return expect_applied(&rec, run_attempt(cx, &rec, payload).await?);
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

    match rec.kind.retry_policy() {
        RetryPolicy::Retry if can_dispatch => {
            let Some(payload) = payload(cx, &rec)? else {
                return unreconcilable(cx, report, &rec, "its request payload is no longer available");
            };
            decide(cx, report, &rec, Decision::Redispatch, "no receipt; retry policy Retry: safe to run again")?;
            expect_applied(&rec, run_attempt(cx, &rec, payload).await?)
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
    match cx.exec.reconcile(&request(&rec, payload.clone(), &cx.contract), &ctx).await {
        Reconciliation::Applied(out) if usable(&rec, &out) => {
            let reason = "no receipt; reconciliation found the effect applied";
            decide(cx, report, &rec, Decision::PublishReconciled, reason)?;
            expect_applied(&rec, finish_attempt(cx, &rec, &out)?)
        }
        Reconciliation::Applied(_) => unreconcilable(cx, report, &rec, "reconciliation returned an unusable outcome"),
        Reconciliation::NotApplied if can_dispatch => {
            let reason = "no receipt; reconciliation found it not applied, so retry policy ReconcileThenRetry retries it";
            decide(cx, report, &rec, Decision::Redispatch, reason)?;
            expect_applied(&rec, run_attempt(cx, &rec, payload).await?)
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
    decide(cx, report, rec, Decision::Abandon, reason)?;
    cx.db.abandon_effect(&rec.effect_id, reason)?;
    report.abandoned.push(rec.effect_id.clone());
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
    if rec.state == EffectState::Dispatched {
        cx.db.mark_unknown(&rec.effect_id)?;
    }
    if !terminal {
        fail(cx.db, &cx.task, &format!("unreconcilable effect {}", rec.effect_id))?;
    }
    Ok(())
}
