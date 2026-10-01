use std::fmt;

use serde::{Deserialize, Serialize};

use crate::contract::Capability;
use crate::ids::{Digest, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    Failure(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetryPolicy {
    Retry,
    ReconcileThenRetry,
    NoRetry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    ReadSnapshot,
    ApplyPatch { expected_base: Digest },
    RunVerification,
    ExportBundle,
}

impl EffectKind {
    pub fn capability(&self) -> Capability {
        match self {
            EffectKind::ReadSnapshot => Capability::SnapshotRead,
            EffectKind::ApplyPatch { .. } => Capability::WorkspaceApplyPatch,
            EffectKind::RunVerification => Capability::VerificationRun,
            EffectKind::ExportBundle => Capability::ArtifactExport,
        }
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        match self {
            EffectKind::ReadSnapshot | EffectKind::RunVerification => RetryPolicy::Retry,
            // Safe to retry after reconciling because of the expected-version check.
            EffectKind::ApplyPatch { .. } | EffectKind::ExportBundle => {
                RetryPolicy::ReconcileThenRetry
            }
        }
    }

    /// Stable short name, used in id derivation and storage.
    pub fn tag(&self) -> &'static str {
        match self {
            EffectKind::ReadSnapshot => "read_snapshot",
            EffectKind::ApplyPatch { .. } => "apply_patch",
            EffectKind::RunVerification => "run_verification",
            EffectKind::ExportBundle => "export_bundle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectState {
    Intended,
    Dispatched,
    Completed,
    Failed,
    Unknown,
    /// Never took effect and never will: the task can no longer dispatch it. Its
    /// reservation is released.
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectId(String);

fn put(h: &mut blake3::Hasher, field: &[u8]) {
    h.update(&(field.len() as u64).to_le_bytes());
    h.update(field);
}

impl EffectId {
    /// Deterministic ID so a restarted controller recomputes the same value.
    /// Every variable-length field is length-prefixed to rule out boundary shifting.
    pub fn derive(
        task_id: &TaskId,
        step: u32,
        kind: &EffectKind,
        request_digest: &Digest,
    ) -> EffectId {
        let mut h = blake3::Hasher::new();
        put(&mut h, task_id.as_str().as_bytes());
        put(&mut h, &step.to_le_bytes());
        put(&mut h, kind.tag().as_bytes());
        if let EffectKind::ApplyPatch { expected_base } = kind {
            put(&mut h, expected_base.to_string().as_bytes());
        }
        put(&mut h, request_digest.as_bytes());
        EffectId(h.finalize().to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptId(String);

impl AttemptId {
    pub fn new() -> AttemptId {
        AttemptId(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for AttemptId {
    fn default() -> Self {
        AttemptId::new()
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub effect_id: EffectId,
    pub task_id: TaskId,
    pub step: u32,
    pub kind: EffectKind,
    pub state: EffectState,
    pub request_digest: Digest,
    pub lease_generation: u64,
    pub result_digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub effect_id: EffectId,
    pub attempt_id: AttemptId,
    pub lease_generation: u64,
    pub outcome: Outcome,
    pub result_digest: Option<Digest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptVerdict {
    Apply,
    DuplicateIgnored,
    StaleLeaseIgnored,
    WrongEffect,
    /// The effect was never dispatched, so no worker can legitimately hold a receipt for it.
    NotDispatched,
}

pub fn accept_receipt(effect: &EffectRecord, r: &Receipt) -> ReceiptVerdict {
    if r.effect_id != effect.effect_id {
        return ReceiptVerdict::WrongEffect;
    }
    if matches!(effect.state, EffectState::Completed | EffectState::Failed) {
        return ReceiptVerdict::DuplicateIgnored;
    }
    // An abandoned effect is closed without ever having taken effect; no attempt of it may
    // complete it.
    if matches!(effect.state, EffectState::Intended | EffectState::Abandoned) {
        return ReceiptVerdict::NotDispatched;
    }
    if r.lease_generation < effect.lease_generation {
        return ReceiptVerdict::StaleLeaseIgnored;
    }
    ReceiptVerdict::Apply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Capability;
    use crate::ids::{Digest, TaskId};

    fn tid(s: &str) -> TaskId {
        serde_json::from_value(serde_json::Value::String(s.into())).unwrap()
    }

    fn id_of(task: &str, step: u32, kind: &EffectKind, req: &[u8]) -> EffectId {
        EffectId::derive(&tid(task), step, kind, &Digest::of(req))
    }

    fn record(state: EffectState, lease: u64) -> EffectRecord {
        let id = id_of("t", 1, &EffectKind::ReadSnapshot, b"r");
        EffectRecord {
            effect_id: id,
            task_id: tid("t"),
            step: 1,
            kind: EffectKind::ReadSnapshot,
            state,
            request_digest: Digest::of(b"r"),
            lease_generation: lease,
            result_digest: None,
        }
    }

    fn receipt(effect_id: EffectId, lease: u64) -> Receipt {
        Receipt {
            effect_id,
            attempt_id: AttemptId::new(),
            lease_generation: lease,
            outcome: Outcome::Success,
            result_digest: Some(Digest::of(b"out")),
        }
    }

    #[test]
    fn same_inputs_same_id_and_is_hex() {
        let a = id_of("t", 1, &EffectKind::RunVerification, b"r");
        let b = id_of("t", 1, &EffectKind::RunVerification, b"r");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    }

    #[test]
    fn any_changed_field_changes_id() {
        let base = id_of("t", 1, &EffectKind::RunVerification, b"r");
        assert_ne!(base, id_of("u", 1, &EffectKind::RunVerification, b"r"));
        assert_ne!(base, id_of("t", 2, &EffectKind::RunVerification, b"r"));
        assert_ne!(base, id_of("t", 1, &EffectKind::ReadSnapshot, b"r"));
        assert_ne!(base, id_of("t", 1, &EffectKind::RunVerification, b"s"));
    }

    #[test]
    fn every_kind_has_distinct_id() {
        let b = Digest::of(b"x");
        let kinds = [
            EffectKind::ReadSnapshot,
            EffectKind::ApplyPatch { expected_base: b },
            EffectKind::RunVerification,
            EffectKind::ExportBundle,
        ];
        let ids: Vec<_> = kinds.iter().map(|k| id_of("t", 1, k, b"r")).collect();
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                assert_ne!(ids[i], ids[j]);
            }
        }
    }

    #[test]
    fn apply_patch_expected_base_changes_id() {
        let k1 = EffectKind::ApplyPatch { expected_base: Digest::of(b"a") };
        let k2 = EffectKind::ApplyPatch { expected_base: Digest::of(b"b") };
        assert_ne!(id_of("t", 1, &k1, b"r"), id_of("t", 1, &k2, b"r"));
        assert_eq!(id_of("t", 1, &k1, b"r"), id_of("t", 1, &k1, b"r"));
    }

    #[test]
    fn field_boundary_shift_changes_id() {
        // Concatenations would collide without length prefixes.
        let k = EffectKind::ReadSnapshot;
        assert_ne!(id_of("ab", 1, &k, b"r"), id_of("a", 1, &k, b"r"));
        // task_id "a" + step bytes vs task_id "a\x01" + shifted step bytes
        assert_ne!(
            id_of("a", 0x0000_0100, &k, b"r"),
            id_of("a\u{0}", 0x0001_0000, &k, b"r")
        );
        assert_ne!(
            id_of("a\u{1}", 0, &k, b"r"),
            id_of("a", 1, &k, b"r")
        );
    }

    #[test]
    fn kind_maps_to_capability() {
        let d = Digest::of(b"x");
        assert_eq!(EffectKind::ReadSnapshot.capability(), Capability::SnapshotRead);
        assert_eq!(
            EffectKind::ApplyPatch { expected_base: d }.capability(),
            Capability::WorkspaceApplyPatch
        );
        assert_eq!(EffectKind::RunVerification.capability(), Capability::VerificationRun);
        assert_eq!(EffectKind::ExportBundle.capability(), Capability::ArtifactExport);
    }

    #[test]
    fn kind_retry_policies() {
        let d = Digest::of(b"x");
        assert_eq!(EffectKind::ReadSnapshot.retry_policy(), RetryPolicy::Retry);
        assert_eq!(
            EffectKind::ApplyPatch { expected_base: d }.retry_policy(),
            RetryPolicy::ReconcileThenRetry
        );
        assert_eq!(EffectKind::RunVerification.retry_policy(), RetryPolicy::Retry);
        assert_eq!(EffectKind::ExportBundle.retry_policy(), RetryPolicy::ReconcileThenRetry);
    }

    #[test]
    fn receipt_applies_when_fresh() {
        let e = record(EffectState::Dispatched, 3);
        assert_eq!(accept_receipt(&e, &receipt(e.effect_id.clone(), 3)), ReceiptVerdict::Apply);
        assert_eq!(accept_receipt(&e, &receipt(e.effect_id.clone(), 4)), ReceiptVerdict::Apply);
    }

    #[test]
    fn receipt_on_completed_or_failed_is_duplicate() {
        for s in [EffectState::Completed, EffectState::Failed] {
            let e = record(s, 3);
            assert_eq!(
                accept_receipt(&e, &receipt(e.effect_id.clone(), 3)),
                ReceiptVerdict::DuplicateIgnored
            );
        }
    }

    #[test]
    fn receipt_with_older_lease_is_stale() {
        let e = record(EffectState::Dispatched, 3);
        assert_eq!(
            accept_receipt(&e, &receipt(e.effect_id.clone(), 2)),
            ReceiptVerdict::StaleLeaseIgnored
        );
    }

    #[test]
    fn receipt_with_other_id_is_wrong_effect() {
        let e = record(EffectState::Dispatched, 3);
        let other = id_of("t", 9, &EffectKind::ReadSnapshot, b"r");
        assert_eq!(accept_receipt(&e, &receipt(other, 3)), ReceiptVerdict::WrongEffect);
    }

    #[test]
    fn verdict_check_order() {
        let other = id_of("t", 9, &EffectKind::ReadSnapshot, b"r");
        // wrong id beats stale lease and completed state
        let e = record(EffectState::Completed, 3);
        assert_eq!(accept_receipt(&e, &receipt(other, 1)), ReceiptVerdict::WrongEffect);
        // duplicate beats stale lease
        assert_eq!(
            accept_receipt(&e, &receipt(e.effect_id.clone(), 1)),
            ReceiptVerdict::DuplicateIgnored
        );
    }

    #[test]
    fn receipt_for_never_dispatched_effect_is_rejected() {
        let e = record(EffectState::Intended, 0);
        for lease in [0, 1, 7] {
            assert_eq!(
                accept_receipt(&e, &receipt(e.effect_id.clone(), lease)),
                ReceiptVerdict::NotDispatched
            );
        }
        // wrong id still wins
        let other = id_of("t", 9, &EffectKind::ReadSnapshot, b"r");
        assert_eq!(accept_receipt(&e, &receipt(other, 0)), ReceiptVerdict::WrongEffect);
    }

    #[test]
    fn receipt_for_abandoned_effect_is_rejected_as_never_dispatched() {
        let e = record(EffectState::Abandoned, 1);
        for lease in [0, 1, 2] {
            assert_eq!(
                accept_receipt(&e, &receipt(e.effect_id.clone(), lease)),
                ReceiptVerdict::NotDispatched
            );
        }
    }

    #[test]
    fn late_receipt_for_unknown_effect_applies() {
        let e = record(EffectState::Unknown, 2);
        assert_eq!(accept_receipt(&e, &receipt(e.effect_id.clone(), 2)), ReceiptVerdict::Apply);
        assert_eq!(
            accept_receipt(&e, &receipt(e.effect_id.clone(), 1)),
            ReceiptVerdict::StaleLeaseIgnored
        );
    }

    #[test]
    fn verdict_serializes_as_its_name() {
        assert_eq!(
            serde_json::to_value(ReceiptVerdict::StaleLeaseIgnored).unwrap(),
            serde_json::json!("StaleLeaseIgnored")
        );
    }

    #[test]
    fn attempt_id_unique_uuid_v4() {
        let (a, b) = (AttemptId::new(), AttemptId::new());
        assert_ne!(a, b);
        let u = uuid::Uuid::parse_str(&a.to_string()).unwrap();
        assert_eq!(u.get_version_num(), 4);
    }

    #[test]
    fn effect_record_and_receipt_serde_round_trip() {
        let mut e = record(EffectState::Completed, 2);
        e.kind = EffectKind::ApplyPatch { expected_base: Digest::of(b"b") };
        e.result_digest = Some(Digest::of(b"o"));
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<EffectRecord>(&json).unwrap(), e);

        let mut r = receipt(e.effect_id.clone(), 2);
        r.outcome = Outcome::Failure("boom".into());
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Receipt>(&json).unwrap(), r);
    }
}
