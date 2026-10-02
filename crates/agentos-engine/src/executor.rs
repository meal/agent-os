//! Backend-neutral execution interface. The fixture executor implements it for local
//! directories; VM and Wasm backends plug in here later.

use std::future::Future;

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome, Receipt};
use agentos_core::ids::{Digest, TaskId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub task_id: TaskId,
    pub kind: EffectKind,
    /// Patch text bytes for `ApplyPatch`, empty otherwise.
    pub payload: Vec<u8>,
    pub contract: Contract,
    /// The task's deadline in unix seconds, 0 for none.
    pub deadline_ts: i64,
}

#[derive(Debug, Clone)]
pub struct AttemptCtx {
    pub attempt_id: AttemptId,
    pub lease_generation: u64,
    pub worker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub passed: bool,
    /// The workspace digest the check ran against.
    pub workspace: Digest,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutcome {
    pub receipt: Receipt,
    /// Bytes of the result artifact; failures carry a JSON description.
    pub output: Vec<u8>,
    /// Set after a successful `ReadSnapshot` or `ApplyPatch`.
    pub new_workspace: Option<Digest>,
    /// Set when a verification ran to completion.
    pub verification: Option<VerificationReport>,
    /// The effect may or may not have taken effect and reconciliation could not tell: the
    /// receipt is a placeholder that must never be applied. The runner marks the effect
    /// UNKNOWN and fails the task instead.
    #[serde(default)]
    pub unresolved: bool,
}

impl ExecOutcome {
    pub fn success(req: &EffectRequest, ctx: &AttemptCtx, output: Vec<u8>) -> ExecOutcome {
        ExecOutcome::with_outcome(req, ctx, Outcome::Success, output)
    }

    pub fn failure(req: &EffectRequest, ctx: &AttemptCtx, reason: impl Into<String>) -> ExecOutcome {
        let reason = reason.into();
        let output = serde_json::to_vec(&serde_json::json!({
            "effect_id": req.effect_id,
            "kind": req.kind.tag(),
            "outcome": "failure",
            "reason": reason,
        }))
        .expect("failure json serializes");
        ExecOutcome::with_outcome(req, ctx, Outcome::Failure(reason), output)
    }

    /// A placeholder for an attempt whose effect cannot be decided (see `unresolved`).
    pub fn unresolved(req: &EffectRequest, ctx: &AttemptCtx, reason: impl Into<String>) -> ExecOutcome {
        let mut out = ExecOutcome::failure(req, ctx, reason);
        out.unresolved = true;
        out
    }

    fn with_outcome(req: &EffectRequest, ctx: &AttemptCtx, outcome: Outcome, output: Vec<u8>) -> ExecOutcome {
        ExecOutcome {
            receipt: Receipt {
                effect_id: req.effect_id.clone(),
                attempt_id: ctx.attempt_id.clone(),
                lease_generation: ctx.lease_generation,
                outcome,
                result_digest: Some(Digest::of(&output)),
            },
            output,
            new_workspace: None,
            verification: None,
            unresolved: false,
        }
    }
}

/// What waiting for an effect's jobs found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobWait {
    /// Every attempt is dead; this is the receipt of the highest lease generation.
    Receipt(Box<ExecOutcome>),
    /// Every attempt is dead and none left a receipt (or there is no attempt at all).
    Dead,
    /// Some attempt still holds its lock after the bound.
    StillAlive,
}

/// What an executor can tell about a dispatched effect it holds no receipt for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation {
    /// The effect provably did not take effect; a new attempt is safe.
    NotApplied,
    /// The effect provably took effect; this is the outcome it had.
    Applied(ExecOutcome),
    /// Cannot tell. The effect must be treated as possibly applied.
    Unknown,
}

pub trait Executor {
    /// Runs one attempt of an effect. Never panics on effect failure: every failure is an
    /// `Outcome::Failure` receipt with a JSON description in `output`.
    fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send;

    /// The latest outcome the executor durably retained for `effect` (highest lease
    /// generation), independent of the controller's database. Recovery publishes it instead
    /// of running the effect again.
    fn retained_outcome(&self, _effect: &EffectId) -> Option<ExecOutcome> {
        None
    }

    /// Inspects the world to decide whether a dispatched effect without a receipt took
    /// effect. `ctx` is the attempt an `Applied` outcome's receipt is issued under.
    fn reconcile(&self, _req: &EffectRequest, _ctx: &AttemptCtx) -> impl Future<Output = Reconciliation> + Send {
        async { Reconciliation::Unknown }
    }

    /// The current digest of `task`'s workspace, `Err` when it cannot be read (e.g. it is
    /// gone), or `None` when this executor cannot tell.
    fn current_workspace(&self, _task: &TaskId) -> Option<Result<Digest, String>> {
        None
    }

    /// Waits, bounded by the executor's own lease bounds, until no attempt of `effect` can
    /// run any more. An executor without out-of-process jobs has nothing to wait for.
    fn await_job(&self, _effect: &EffectId) -> impl Future<Output = JobWait> + Send {
        async { JobWait::Dead }
    }

    /// Stops every live attempt of `effect` and returns whether all of them are dead. An
    /// executor without out-of-process jobs has nothing to stop.
    fn fence_job(&self, _effect: &EffectId) -> impl Future<Output = bool> + Send {
        async { true }
    }
}
