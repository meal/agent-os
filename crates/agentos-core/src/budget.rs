use serde::{Deserialize, Serialize};

use crate::contract::Limits;
use crate::effect::EffectKind;

/// Resources an effect intent reserves before it may be dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Reservation {
    pub tool_actions: u32,
    pub model_requests: u32,
}

impl Reservation {
    /// The reservation shape a kind requires: `tool_actions` is fixed by the kind,
    /// `model_requests` is chosen by the caller.
    pub fn for_kind(kind: &EffectKind, model_requests: u32) -> Reservation {
        Reservation {
            tool_actions: tool_actions_for(kind),
            model_requests,
        }
    }
}

/// Only snapshot reads and patch applications consume a tool action; verification and
/// export are controller work, not agent actions.
pub fn tool_actions_for(kind: &EffectKind) -> u32 {
    match kind {
        EffectKind::ReadSnapshot | EffectKind::ApplyPatch { .. } => 1,
        EffectKind::RunVerification | EffectKind::ExportBundle => 0,
    }
}

/// Model-request usage of a task, bucketed by usage-row status (each row counted once).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    pub reserved_model_requests: u64,
    pub settled_model_requests: u64,
    pub uncertain_model_requests: u64,
}

impl UsageTotals {
    pub fn committed(&self) -> u64 {
        self.reserved_model_requests
            .saturating_add(self.settled_model_requests)
            .saturating_add(self.uncertain_model_requests)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum BudgetError {
    #[error("model request limit {limit} exceeded: {committed} committed, {requested} requested")]
    ModelRequests { limit: u32, committed: u64, requested: u32 },
    #[error("tool action limit of {limit} exhausted")]
    ToolActions { limit: u32 },
}

/// Reserved, settled and uncertain requests all count: an uncertain effect may have run.
pub fn check_model_budget(
    totals: &UsageTotals,
    reserve: &Reservation,
    limits: &Limits,
) -> Result<(), BudgetError> {
    let committed = totals.committed();
    if committed.saturating_add(u64::from(reserve.model_requests)) > u64::from(limits.model_requests) {
        return Err(BudgetError::ModelRequests {
            limit: limits.model_requests,
            committed,
            requested: reserve.model_requests,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Digest;

    fn limits(model_requests: u32) -> Limits {
        Limits {
            model_requests,
            max_output_tokens_per_request: 1000,
            tool_actions: 5,
            deadline_seconds: 60,
            worker_vcpus: 1,
            worker_memory_mib: 256,
        }
    }

    fn totals(r: u64, s: u64, u: u64) -> UsageTotals {
        UsageTotals {
            reserved_model_requests: r,
            settled_model_requests: s,
            uncertain_model_requests: u,
        }
    }

    fn req(n: u32) -> Reservation {
        Reservation { tool_actions: 0, model_requests: n }
    }

    #[test]
    fn exactly_reaching_the_limit_is_allowed() {
        assert_eq!(check_model_budget(&totals(1, 1, 1), &req(2), &limits(5)), Ok(()));
        assert_eq!(check_model_budget(&UsageTotals::default(), &req(5), &limits(5)), Ok(()));
    }

    #[test]
    fn every_bucket_counts_toward_the_limit() {
        for t in [totals(5, 0, 0), totals(0, 5, 0), totals(0, 0, 5)] {
            assert_eq!(
                check_model_budget(&t, &req(1), &limits(5)),
                Err(BudgetError::ModelRequests { limit: 5, committed: 5, requested: 1 }),
                "{t:?}"
            );
        }
    }

    #[test]
    fn zero_request_reservation_fits_a_full_budget() {
        assert_eq!(check_model_budget(&totals(2, 2, 1), &req(0), &limits(5)), Ok(()));
    }

    #[test]
    fn huge_totals_saturate_instead_of_overflowing() {
        let t = totals(u64::MAX, u64::MAX, 1);
        assert_eq!(t.committed(), u64::MAX);
        assert!(check_model_budget(&t, &req(u32::MAX), &limits(u32::MAX)).is_err());
    }

    #[test]
    fn only_snapshot_and_patch_consume_tool_actions() {
        let d = Digest::of(b"x");
        assert_eq!(tool_actions_for(&EffectKind::ReadSnapshot), 1);
        assert_eq!(tool_actions_for(&EffectKind::ApplyPatch { expected_base: d }), 1);
        assert_eq!(tool_actions_for(&EffectKind::RunVerification), 0);
        assert_eq!(tool_actions_for(&EffectKind::ExportBundle), 0);
        assert_eq!(
            Reservation::for_kind(&EffectKind::ReadSnapshot, 3),
            Reservation { tool_actions: 1, model_requests: 3 }
        );
    }
}
