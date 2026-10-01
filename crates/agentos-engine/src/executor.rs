//! Backend-neutral execution interface. The fixture executor implements it for local
//! directories; VM and Wasm backends plug in here later.

use std::future::Future;

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome, Receipt};
use agentos_core::ids::{Digest, TaskId};

#[derive(Debug, Clone)]
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub task_id: TaskId,
    pub kind: EffectKind,
    /// Patch text bytes for `ApplyPatch`, empty otherwise.
    pub payload: Vec<u8>,
    pub contract: Contract,
}

#[derive(Debug, Clone)]
pub struct AttemptCtx {
    pub attempt_id: AttemptId,
    pub lease_generation: u64,
    pub worker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub passed: bool,
    /// The workspace digest the check ran against.
    pub workspace: Digest,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub receipt: Receipt,
    /// Bytes of the result artifact; failures carry a JSON description.
    pub output: Vec<u8>,
    /// Set after a successful `ReadSnapshot` or `ApplyPatch`.
    pub new_workspace: Option<Digest>,
    /// Set when a verification ran to completion.
    pub verification: Option<VerificationReport>,
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
        }
    }
}

pub trait Executor {
    /// Runs one attempt of an effect. Never panics on effect failure: every failure is an
    /// `Outcome::Failure` receipt with a JSON description in `output`.
    fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send;
}
