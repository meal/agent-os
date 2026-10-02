//! The steps every effect goes through, each committed before the next starts:
//! [`intend`] -> [`dispatch`] -> [`execute`] -> [`store_result`] -> [`register_result`] ->
//! [`complete`]. A crash between any two leaves state that recovery resumes from; the
//! crash hook is consulted at each of those boundaries ([`run_attempt`], [`finish_attempt`]).

use agentos_core::broker::Resource;
use agentos_core::budget::Reservation;
use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectKind, EffectRecord, EffectState, Outcome, ReceiptVerdict};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{Task, TaskEvent, TaskState};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use serde_json::json;

use crate::crash::{CrashPoint, RunOptions};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::runner::{fail, EngineError, Result};

pub const WORKER: &str = "fixture-executor";

/// Everything one run (or recovery) of a task works with.
pub(crate) struct Cx<'a, E> {
    pub db: &'a Db,
    pub blobs: &'a BlobStore,
    pub exec: &'a E,
    pub contract: Contract,
    pub task: TaskId,
    pub opts: &'a RunOptions,
    /// Set in closing mode: recovery publishes and reconciles what is in flight, dispatches
    /// nothing, and fails the task with this reason (see [`Cx::closing_with`]).
    pub closing: Option<String>,
}

impl<'a, E> Cx<'a, E> {
    pub fn new(db: &'a Db, blobs: &'a BlobStore, exec: &'a E, task: &TaskId, opts: &'a RunOptions) -> Result<Self> {
        Ok(Cx { db, blobs, exec, contract: db.contract(task)?, task: task.clone(), opts, closing: None })
    }

    /// The same run in closing mode: the task is being ended with `reason`.
    pub fn closing_with(&self, reason: &str) -> Cx<'a, E> {
        Cx {
            db: self.db,
            blobs: self.blobs,
            exec: self.exec,
            contract: self.contract.clone(),
            task: self.task.clone(),
            opts: self.opts,
            closing: Some(reason.to_string()),
        }
    }

    /// `Err(Crashed(point))` when the crash hook fires here.
    pub fn crash(&self, point: CrashPoint, kind: Option<&'static str>) -> Result<()> {
        if self.opts.crashes_at(point, kind) {
            return Err(EngineError::Crashed(point));
        }
        Ok(())
    }
}

/// Step 1: durably record the intent (and its reservation) before anything runs. The store
/// authorizes the effect's capability on `resource` and journals that decision.
pub fn intend(
    db: &Db,
    task: &TaskId,
    kind: EffectKind,
    request: Digest,
    expected_workspace: &Digest,
    resource: &Resource,
) -> Result<EffectRecord> {
    let reserve = Reservation::for_kind(&kind, 0);
    let rec = db.record_intent(task, kind, request, expected_workspace, reserve, resource)?;
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

/// `deadline_ts` is the task's deadline in unix seconds (`Db::deadline_ts`), 0 for none.
pub fn request(rec: &EffectRecord, payload: Vec<u8>, contract: &Contract, deadline_ts: i64) -> EffectRequest {
    EffectRequest {
        effect_id: rec.effect_id.clone(),
        task_id: rec.task_id.clone(),
        kind: rec.kind.clone(),
        payload,
        contract: contract.clone(),
        deadline_ts,
    }
}

/// An outcome is usable only if its receipt names `rec` and describes its own output.
pub fn check_outcome(rec: &EffectRecord, out: &ExecOutcome) -> Result<()> {
    if out.receipt.effect_id != rec.effect_id {
        let msg = format!("receipt for {} returned for {}", out.receipt.effect_id, rec.effect_id);
        return Err(EngineError::Protocol(msg));
    }
    if out.receipt.result_digest != Some(Digest::of(&out.output)) {
        let msg = format!("receipt for {} does not describe its output", rec.effect_id);
        return Err(EngineError::Protocol(msg));
    }
    Ok(())
}

/// Step 3: run the attempt on the executor. An `unresolved` outcome is returned as it is;
/// [`run_attempt`] turns it into an UNKNOWN effect and a failed task.
pub async fn execute<E: Executor>(
    executor: &E,
    rec: &EffectRecord,
    payload: Vec<u8>,
    contract: &Contract,
    deadline_ts: i64,
    ctx: &AttemptCtx,
) -> Result<ExecOutcome> {
    let out = executor.run(&request(rec, payload, contract, deadline_ts), ctx).await;
    check_outcome(rec, &out)?;
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

/// Step 4: write the result bytes to the blob store.
pub fn store_result(blobs: &BlobStore, out: &ExecOutcome) -> Result<Digest> {
    Ok(blobs.put(&out.output)?)
}

/// Step 5: register the stored result as the effect's artifact.
pub fn register_result(db: &Db, rec: &EffectRecord, out: &ExecOutcome, digest: &Digest) -> Result<()> {
    let provenance = json!({ "worker": WORKER, "attempt_id": out.receipt.attempt_id, "effect_id": rec.effect_id });
    db.register_artifact(
        digest,
        out.output.len() as u64,
        artifact_type(&rec.kind, &out.receipt.outcome),
        Some(&rec.effect_id),
        &provenance.to_string(),
    )?;
    Ok(())
}

/// Step 6: apply the receipt together with the follow-up task event. Returns the store's
/// verdict; anything but `Apply` changed nothing but the audit journal.
pub fn complete(
    db: &Db,
    rec: &EffectRecord,
    out: &ExecOutcome,
    artifact: &Digest,
    follow_up: Option<TaskEvent>,
) -> Result<ReceiptVerdict> {
    let verdict = db.complete_effect(&rec.effect_id, &out.receipt, Some(artifact), follow_up)?;
    tracing::info!(task_id = %rec.task_id, effect_id = %rec.effect_id, ?verdict, "receipt applied");
    Ok(verdict)
}

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// Its outcome was published; the store's verdict on the receipt.
    Published(ReceiptVerdict),
    /// The attempt ended the task instead: the executor could not tell whether the effect
    /// took effect (the effect is UNKNOWN and the task failed), or the task's deadline had
    /// passed before dispatch (the task is failed). The task is in this state now.
    Ended(TaskState),
}

/// Steps 2-6 for an effect that is INTENDED, or DISPATCHED/UNKNOWN without a usable
/// receipt: a new attempt under the next lease generation.
pub(crate) async fn run_attempt<E: Executor>(cx: &Cx<'_, E>, rec: &EffectRecord, payload: Vec<u8>) -> Result<Attempt> {
    let kind = Some(rec.kind.tag());
    // Before dispatch: past the deadline nothing new runs. In closing mode this effect is
    // abandoned (it is still INTENDED) or decided like any in-flight one.
    if let Some(state) = crate::recover::deadline_stop(cx).await? {
        return Ok(Attempt::Ended(state));
    }
    let ctx = dispatch(cx.db, rec)?;
    cx.crash(CrashPoint::AfterDispatch, kind)?;
    let deadline_ts = cx.db.deadline_ts(&rec.task_id)?;
    let out = execute(cx.exec, rec, payload, &cx.contract, deadline_ts, &ctx).await?;
    if let Some(point) = cx.opts.tripped() {
        // The controller "died" inside the executor, right after launching the job.
        return Err(EngineError::Crashed(point));
    }
    if out.unresolved {
        tracing::warn!(task_id = %rec.task_id, effect_id = %rec.effect_id, "the executor cannot tell whether the effect took effect");
        return Ok(Attempt::Ended(mark_unreconcilable(cx.db, &cx.db.effect(&rec.effect_id)?)?));
    }
    cx.crash(CrashPoint::AfterExecuteBeforePublish, kind)?;
    Ok(Attempt::Published(finish_attempt(cx, rec, &out)?))
}

/// The end of an effect nobody can decide: a DISPATCHED one becomes UNKNOWN (its reservation
/// `Uncertain`, since it may have run), and a task that is not terminal yet fails with
/// "unreconcilable effect <id>". Returns the task's state afterwards.
pub(crate) fn mark_unreconcilable(db: &Db, rec: &EffectRecord) -> Result<TaskState> {
    if rec.state == EffectState::Dispatched {
        db.mark_unknown(&rec.effect_id)?;
    }
    let t = db.task(&rec.task_id)?;
    if t.state.is_terminal() {
        return Ok(t.state);
    }
    fail(db, &rec.task_id, &format!("unreconcilable effect {}", rec.effect_id))
}

/// Steps 4-6 for an outcome in hand, whether just executed, retained by the executor
/// across a crash, or found by reconciliation.
pub(crate) fn finish_attempt<E>(cx: &Cx<'_, E>, rec: &EffectRecord, out: &ExecOutcome) -> Result<ReceiptVerdict> {
    let kind = Some(rec.kind.tag());
    let artifact = store_result(cx.blobs, out)?;
    cx.crash(CrashPoint::AfterBlobPut, kind)?;
    register_result(cx.db, rec, out, &artifact)?;
    cx.crash(CrashPoint::AfterRegister, kind)?;
    let task = cx.db.task(&rec.task_id)?;
    let verdict = complete(cx.db, rec, out, &artifact, follow_up_event(&rec.kind, out, &task))?;
    if verdict == ReceiptVerdict::Apply {
        cx.crash(CrashPoint::AfterComplete, kind)?;
    }
    Ok(verdict)
}

/// The task event that completes an effect of `kind` with outcome `out`, given `task` as
/// it is right before completion. Recovery reuses this to finish effects after a crash.
///
/// - ReadSnapshot / ApplyPatch: `WorkspaceUpdated` with the new digest on success, else none.
///   It is not filtered by task state: the store applies it (Running or Paused), journals it
///   as rejected when a cancel is pending or the task is terminal, and errors otherwise.
/// - RunVerification: `VerifyPassed` for the task's current digest only when the check ran,
///   passed, and its evidence is for exactly that digest; otherwise `VerifyFailed`.
/// - ExportBundle: none.
pub fn follow_up_event(kind: &EffectKind, out: &ExecOutcome, task: &Task) -> Option<TaskEvent> {
    match kind {
        EffectKind::ReadSnapshot | EffectKind::ApplyPatch { .. } => match (&out.receipt.outcome, out.new_workspace) {
            (Outcome::Success, Some(digest)) => Some(TaskEvent::WorkspaceUpdated { digest }),
            _ => None,
        },
        EffectKind::RunVerification => Some(if verification_verdict(out, task).0 {
            TaskEvent::VerifyPassed { digest: task.workspace_digest }
        } else {
            TaskEvent::VerifyFailed
        }),
        EffectKind::ExportBundle => None,
    }
}

/// Whether a verification outcome proves `task`'s current workspace, and a summary for the
/// agent. Evidence counts only for the digest the task holds right now.
pub fn verification_verdict(out: &ExecOutcome, task: &Task) -> (bool, String) {
    match (&out.receipt.outcome, &out.verification) {
        (Outcome::Success, Some(r)) if r.workspace == task.workspace_digest => (r.passed, r.summary.clone()),
        (Outcome::Success, Some(r)) => {
            (false, format!("evidence is for workspace {}, task has {}", r.workspace, task.workspace_digest))
        }
        (Outcome::Success, None) => (false, "executor returned no verification report".into()),
        (Outcome::Failure(reason), _) => (false, format!("verification did not complete: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use agentos_core::effect::{AttemptId, EffectId};

    use super::*;
    use crate::executor::VerificationReport;

    fn task(state: TaskState, workspace: Digest) -> Task {
        Task { state, ..Task::new(TaskId::new(), workspace) }
    }

    fn outcome(kind: &EffectKind, ok: bool) -> ExecOutcome {
        let req = EffectRequest {
            effect_id: EffectId::derive(&TaskId::new(), 0, kind, &Digest::of(b"r")),
            task_id: TaskId::new(),
            kind: kind.clone(),
            payload: Vec::new(),
            contract: serde_json::from_value(json!({
                "goal": "g", "repository": {"source": "s", "revision": "r"}, "profile": "p",
                "editable_paths": ["src/**"], "verification_profile": "v", "capabilities": [],
                "limits": {"model_requests": 1, "max_output_tokens_per_request": 1, "tool_actions": 1,
                           "deadline_seconds": 1, "worker_vcpus": 1, "worker_memory_mib": 1}
            }))
            .unwrap(),
            deadline_ts: 0,
        };
        let ctx = AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "w".into() };
        if ok {
            ExecOutcome::success(&req, &ctx, b"{}".to_vec())
        } else {
            ExecOutcome::failure(&req, &ctx, "timeout")
        }
    }

    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    #[test]
    fn workspace_changing_effects_follow_up_with_the_new_digest() {
        for kind in [EffectKind::ReadSnapshot, EffectKind::ApplyPatch { expected_base: d("w0") }] {
            let mut out = outcome(&kind, true);
            out.new_workspace = Some(d("w1"));
            // Applied whatever the state: the store decides (Paused accepts it too).
            for state in [TaskState::Running, TaskState::Paused] {
                assert_eq!(
                    follow_up_event(&kind, &out, &task(state, d("w0"))),
                    Some(TaskEvent::WorkspaceUpdated { digest: d("w1") }),
                    "{kind:?} {state:?}"
                );
            }
            assert_eq!(follow_up_event(&kind, &outcome(&kind, false), &task(TaskState::Running, d("w0"))), None);
        }
    }

    #[test]
    fn verification_follow_up_depends_on_the_report_and_the_current_digest() {
        let kind = EffectKind::RunVerification;
        let now = task(TaskState::Verifying, d("w1"));
        let report = |passed: bool, workspace: Digest| {
            let mut out = outcome(&kind, true);
            out.verification = Some(VerificationReport { passed, workspace, summary: "s".into() });
            out
        };
        assert_eq!(follow_up_event(&kind, &report(true, d("w1")), &now), Some(TaskEvent::VerifyPassed { digest: d("w1") }));
        assert_eq!(follow_up_event(&kind, &report(false, d("w1")), &now), Some(TaskEvent::VerifyFailed));
        // Passing evidence for another workspace version is not success.
        assert_eq!(follow_up_event(&kind, &report(true, d("w0")), &now), Some(TaskEvent::VerifyFailed));
        // Timeout (effect failure) and a missing report both fail verification.
        assert_eq!(follow_up_event(&kind, &outcome(&kind, false), &now), Some(TaskEvent::VerifyFailed));
        assert_eq!(follow_up_event(&kind, &outcome(&kind, true), &now), Some(TaskEvent::VerifyFailed));
        let (passed, summary) = verification_verdict(&outcome(&kind, false), &now);
        assert!(!passed && summary.contains("timeout"), "{summary}");
    }

    #[test]
    fn export_has_no_follow_up() {
        let kind = EffectKind::ExportBundle;
        let now = task(TaskState::Running, d("w"));
        assert_eq!(follow_up_event(&kind, &outcome(&kind, false), &now), None);
        assert_eq!(follow_up_event(&kind, &outcome(&kind, true), &now), None);
    }

    #[test]
    fn outcomes_must_name_the_effect_and_describe_their_output() {
        let kind = EffectKind::RunVerification;
        let out = outcome(&kind, true);
        let rec = EffectRecord {
            effect_id: out.receipt.effect_id.clone(),
            task_id: TaskId::new(),
            step: 0,
            kind,
            state: EffectState::Dispatched,
            request_digest: d("r"),
            lease_generation: 1,
            result_digest: None,
        };
        assert!(check_outcome(&rec, &out).is_ok());
        let mut tampered = out.clone();
        tampered.output = b"other".to_vec();
        assert!(matches!(check_outcome(&rec, &tampered), Err(EngineError::Protocol(_))));
        let other = EffectRecord { effect_id: EffectId::derive(&TaskId::new(), 1, &rec.kind, &d("x")), ..rec };
        assert!(matches!(check_outcome(&other, &out), Err(EngineError::Protocol(_))));
    }
}
