use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use agentos_core::broker::Resource;
use agentos_core::budget::{BudgetError, Reservation};
use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{
    AttemptId, EffectId, EffectKind, EffectRecord, EffectState, Outcome, Receipt, ReceiptVerdict,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{Task, TaskEvent, TaskState, TransitionError};
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError, StoredEvent};
use agentos_store::effects::UsageSummary;
use rusqlite::Connection;
use serde_json::json;

const SAME_OUTPUT: &[u8] = b"ok\n";

#[test]
fn model_retry_schedule_is_owned_idempotent_and_validates_the_failed_effect() {
    let fx = setup_with(MODEL_CAPS, 5, 5);
    let kind = call(1);
    let e = fx.db.record_intent(&fx.id, kind.clone(), Digest::of(b"request"), &fx.base(),
        Reservation::for_kind(&kind, 1), &Resource::Task).unwrap();
    assert!(fx.db.schedule_model_retry(&fx.id, &e.effect_id, 2, 2, None, 1).is_err());
    let attempt = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &attempt, "model", 1).unwrap();
    fx.db.complete_effect(&e.effect_id, &receipt(&e, &attempt, 1, Outcome::Failure("busy".into())), None, None).unwrap();
    let scheduled = fx.db.schedule_model_retry(&fx.id, &e.effect_id, 2, 2, None, 1).unwrap();
    let again = fx.reopen().schedule_model_retry(&fx.id, &e.effect_id, 2, 60, Some(i64::MAX), 1).unwrap();
    assert_eq!(again, scheduled);
    assert_eq!(fx.count_events("ModelRetryScheduled"), 1);
    assert!(fx.db.schedule_model_retry(&TaskId::new(), &e.effect_id, 2, 2, None, 1).is_err());
    assert!(fx.db.schedule_model_retry(&fx.id, &e.effect_id, 0, 2, None, 1).is_err());
    assert!(fx.db.schedule_model_retry(&fx.id, &e.effect_id, 2, 2, None, 9).is_err());
    assert!(matches!(fx.db.append_audit(&fx.id, "ModelRetryScheduled", &json!({})), Err(DbError::ReservedEventType(_))));
}

const MODEL_CAPS: &[&str] = &["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export", "model.request"];

fn call(turn: u32) -> EffectKind {
    EffectKind::ModelCall { model: "fake".into(), turn }
}

const ALL_CAPS: &[&str] = &["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"];

fn contract(caps: &[&str], model_requests: u32, tool_actions: u32) -> (Contract, Digest) {
    let json = json!({
        "goal": "fix the parser",
        "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
        "profile": "protected",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": caps,
        "limits": {
            "model_requests": model_requests,
            "max_output_tokens_per_request": 1000,
            "tool_actions": tool_actions,
            "deadline_seconds": 600,
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }
    })
    .to_string();
    (Contract::parse(&json).unwrap(), Digest::of(json.as_bytes()))
}

/// The resources the engine passes for each kind (see the runner).
fn profile() -> Resource {
    Resource::Profile("parser-checks-v1".into())
}

fn src() -> Resource {
    Resource::Paths(vec!["src/a.py".into()])
}

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: Db,
    id: TaskId,
}

impl Fx {
    fn reopen(&self) -> Db {
        Db::open(&self.path).unwrap()
    }
    fn task(&self) -> Task {
        self.db.task(&self.id).unwrap()
    }
    fn base(&self) -> Digest {
        self.task().workspace_digest
    }
    fn events(&self) -> Vec<StoredEvent> {
        self.db.events(&self.id).unwrap()
    }
    fn usage(&self) -> UsageSummary {
        self.db.usage_summary(&self.id).unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        Connection::open(&self.path).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn count_events(&self, event_type: &str) -> usize {
        self.events().iter().filter(|e| e.event_type == event_type).count()
    }
    fn read(&self, req: &[u8], model_requests: u32) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::ReadSnapshot;
        let r = Reservation::for_kind(&kind, model_requests);
        self.db.record_intent(&self.id, kind, Digest::of(req), &self.base(), r, &Resource::Task)
    }
    fn verify(&self, req: &[u8], model_requests: u32) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::RunVerification;
        let r = Reservation::for_kind(&kind, model_requests);
        self.db.record_intent(&self.id, kind, Digest::of(req), &self.base(), r, &profile())
    }
    fn patch(&self, req: &[u8], expected: Digest) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::ApplyPatch { expected_base: expected };
        let r = Reservation::for_kind(&kind, 1);
        self.db.record_intent(&self.id, kind, Digest::of(req), &expected, r, &src())
    }
    /// Intent + dispatch at `lease`; returns the effect and the attempt.
    fn dispatched(&self, req: &[u8], lease: u64) -> (EffectRecord, AttemptId) {
        let e = self.read(req, 1).unwrap();
        let a = AttemptId::new();
        self.db.mark_dispatched(&e.effect_id, &a, "worker-1", lease).unwrap();
        (e, a)
    }
    fn blobs(&self) -> BlobStore {
        BlobStore::open(self.path.parent().unwrap().join("blobs")).unwrap()
    }
    /// Engine publish order, steps 1 and 2: put the blob, then register it for the effect.
    fn publish(&self, e: &EffectRecord, bytes: &[u8]) -> Digest {
        let d = self.blobs().put(bytes).unwrap();
        self.db.register_artifact(&d, bytes.len() as u64, "result", Some(&e.effect_id), "worker-1").unwrap();
        d
    }
    /// Publish a result for `e`, then complete it successfully (step 3).
    fn succeed(
        &self,
        e: &EffectRecord,
        a: &AttemptId,
        lease: u64,
        follow_up: Option<TaskEvent>,
    ) -> Result<ReceiptVerdict, DbError> {
        // Deliberately the same bytes for every effect: content addressing must not tie a
        // blob to the first effect that produced it.
        let art = self.publish(e, SAME_OUTPUT);
        let r = Receipt { result_digest: Some(art), ..receipt(e, a, lease, Outcome::Success) };
        self.db.complete_effect(&e.effect_id, &r, Some(&art), follow_up)
    }
    /// Everything a rejected or ignored call must leave untouched.
    fn snapshot(&self, effect: &EffectId) -> (Task, EffectRecord, UsageSummary, i64) {
        (
            self.task(),
            self.db.effect(effect).unwrap(),
            self.usage(),
            self.count("SELECT count(*) FROM attempts WHERE finished_ts IS NULL"),
        )
    }
}

fn setup_with(caps: &[&str], model_requests: u32, tool_actions: u32) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let db = Db::open(&path).unwrap();
    let (c, d) = contract(caps, model_requests, tool_actions);
    let id = db.create_task(&c, &d).unwrap();
    db.approve_task(&id).unwrap();
    db.append(&id, &TaskEvent::Started).unwrap();
    Fx { _dir: dir, path, db, id }
}

fn setup() -> Fx {
    setup_with(ALL_CAPS, 10, 100)
}

fn receipt(e: &EffectRecord, attempt: &AttemptId, lease: u64, outcome: Outcome) -> Receipt {
    Receipt {
        effect_id: e.effect_id.clone(),
        attempt_id: attempt.clone(),
        lease_generation: lease,
        outcome,
        result_digest: None,
    }
}

fn inject_failure(path: &Path, event_type: &str) {
    let raw = Connection::open(path).unwrap();
    raw.execute_batch(&format!(
        "CREATE TRIGGER fail_ins BEFORE INSERT ON events WHEN NEW.type='{event_type}'
         BEGIN SELECT RAISE(ABORT,'injected'); END;"
    ))
    .unwrap();
}

// ---------- record_intent ----------

#[test]
fn record_intent_twice_is_idempotent() {
    let fx = setup();
    let step_before = fx.task().step;
    let a = fx.read(b"req", 1).unwrap();
    let b = fx.read(b"req", 1).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.state, EffectState::Intended);
    assert_eq!(a.lease_generation, 0);
    assert_eq!(a.step, step_before);
    // The stored step is the one the id was derived from.
    assert_eq!(EffectId::derive(&a.task_id, a.step, &a.kind, &a.request_digest), a.effect_id);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 1);
    assert_eq!(fx.count_events("EffectIntended"), 1);
    assert_eq!(fx.count_events("ActionUsed"), 1);
    assert_eq!(fx.task().actions_used, 1);
    let u = fx.usage();
    assert_eq!((u.reserved_model_requests, u.reserved_tool_actions), (1, 1));
    assert_eq!(fx.db.effect(&a.effect_id).unwrap(), a);
}

#[test]
fn idempotent_retry_survives_reopen_and_ignores_new_reservation() {
    let fx = setup();
    let a = fx.read(b"req", 1).unwrap();
    let db = fx.reopen();
    let kind = EffectKind::ReadSnapshot;
    let again = db
        .record_intent(&fx.id, kind, Digest::of(b"req"), &fx.base(), Reservation { tool_actions: 1, model_requests: 9 }, &Resource::Task)
        .unwrap();
    assert_eq!(again, a);
    assert_eq!(fx.usage().reserved_model_requests, 1);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 1);
}

#[test]
fn same_request_after_an_intervening_state_change_is_a_new_effect() {
    let fx = setup();
    let a = fx.read(b"req", 1).unwrap();
    fx.db.append(&fx.id, &TaskEvent::WorkspaceUpdated { digest: Digest::of(b"w1") }).unwrap();
    let b = fx.read(b"req", 1).unwrap();
    assert_ne!(a.effect_id, b.effect_id);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 2);
    assert_eq!(fx.task().actions_used, 2);
    assert_eq!(EffectId::derive(&b.task_id, b.step, &b.kind, &b.request_digest), b.effect_id);
}

#[test]
fn repeated_intent_after_completion_returns_the_finished_record() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"req", 1);
    fx.succeed(&e, &a, 1, None).unwrap();
    let done = fx.db.effect(&e.effect_id).unwrap();
    assert_eq!(done.state, EffectState::Completed);
    let (task, effects, usage) = (fx.task(), fx.count("SELECT count(*) FROM effects"), fx.usage());
    let events = fx.events();
    // Dispatch, completion and artifact rows do not advance the step, so this is still a retry.
    assert_eq!(fx.read(b"req", 1).unwrap(), done);
    assert_eq!(fx.task(), task);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), effects);
    assert_eq!(fx.usage(), usage);
    assert_eq!(fx.events(), events);
}

#[test]
fn verification_intent_is_idempotent_and_consumes_no_tool_action() {
    let fx = setup();
    let step_before = fx.task().step;
    let a = fx.verify(b"v", 2).unwrap();
    let b = fx.verify(b"v", 2).unwrap();
    assert_eq!(a, b);
    assert_eq!(fx.task().actions_used, 0);
    assert_eq!(fx.task().step, step_before);
    assert_eq!(fx.count_events("ActionUsed"), 0);
    assert_eq!(fx.count_events("EffectIntended"), 1);
    let u = fx.usage();
    assert_eq!((u.reserved_model_requests, u.reserved_tool_actions), (2, 0));
}

#[test]
fn concurrent_identical_intents_create_one_effect() {
    let fx = setup();
    let base = fx.base();
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (path, id, barrier) = (fx.path.clone(), fx.id.clone(), barrier.clone());
            thread::spawn(move || {
                let db = Db::open(&path).unwrap();
                barrier.wait();
                let k = EffectKind::ReadSnapshot;
                let r = Reservation::for_kind(&k, 1);
                db.record_intent(&id, k, Digest::of(b"req"), &base, r, &Resource::Task).unwrap()
            })
        })
        .collect();
    let recs: Vec<EffectRecord> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(recs.iter().all(|r| *r == recs[0]));
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 1);
    assert_eq!(fx.task().actions_used, 1);
}

#[test]
fn model_request_reservation_over_the_limit_is_rejected_and_writes_nothing() {
    let fx = setup_with(ALL_CAPS, 3, 100);
    fx.read(b"a", 2).unwrap();
    let (task, events) = (fx.task(), fx.events());
    let err = fx.read(b"b", 2).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::BudgetExceeded(BudgetError::ModelRequests { limit: 3, committed: 2, requested: 2 })
        ),
        "{err:?}"
    );
    assert_eq!(fx.task(), task);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 1);
}

#[test]
fn settled_usage_counts_once_toward_the_model_limit() {
    let fx = setup_with(ALL_CAPS, 3, 100);
    let (e, a) = fx.dispatched(b"a", 1);
    fx.succeed(&e, &a, 1, None).unwrap();
    assert_eq!(fx.usage().settled_model_requests, 1);
    assert_eq!(fx.usage().reserved_model_requests, 0);
    // 1 settled + 2 new == limit: allowed.
    fx.read(b"b", 2).unwrap();
    assert!(matches!(fx.read(b"c", 1), Err(DbError::BudgetExceeded(BudgetError::ModelRequests { .. }))));
}

#[test]
fn tool_action_limit_surfaces_as_budget_exceeded_and_writes_nothing() {
    let fx = setup_with(ALL_CAPS, 10, 1);
    fx.read(b"a", 0).unwrap();
    let (task, events) = (fx.task(), fx.events());
    let err = fx.read(b"b", 0).unwrap_err();
    assert!(matches!(err, DbError::BudgetExceeded(BudgetError::ToolActions { limit: 1 })), "{err:?}");
    assert_eq!(fx.task(), task);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 1);
    // Non-consuming kinds still fit.
    fx.verify(b"v", 0).unwrap();
}

#[test]
fn reservation_must_match_the_kinds_tool_action_cost() {
    let fx = setup();
    let base = fx.base();
    let err = fx
        .db
        .record_intent(&fx.id, EffectKind::ReadSnapshot, Digest::of(b"r"), &base, Reservation { tool_actions: 0, model_requests: 1 }, &Resource::Task)
        .unwrap_err();
    assert!(matches!(err, DbError::InvalidReservation { expected: 1, got: 0 }), "{err:?}");
    let err = fx
        .db
        .record_intent(&fx.id, EffectKind::ExportBundle, Digest::of(b"r"), &base, Reservation { tool_actions: 1, model_requests: 0 }, &Resource::Task)
        .unwrap_err();
    assert!(matches!(err, DbError::InvalidReservation { expected: 0, got: 1 }), "{err:?}");
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
}

#[test]
fn missing_capability_is_denied_with_a_durable_audit_event() {
    let fx = setup_with(&["snapshot.read", "workspace.apply_patch"], 10, 100);
    let (task, events) = (fx.task(), fx.events());
    let err = fx.verify(b"v", 1).unwrap_err();
    assert!(
        matches!(err, DbError::CapabilityDenied { capability: Capability::VerificationRun, ref reason } if reason == "unknown_handle"),
        "{err:?}"
    );
    assert_eq!(fx.task(), task);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 0);
    let after = fx.reopen().events(&fx.id).unwrap();
    // The broker's journaled decision, then the engine-facing `Denied` audit row.
    assert_eq!(after.len(), events.len() + 2);
    assert_eq!(after[events.len()].event_type, "CapabilityDenied");
    let denied = after.last().unwrap();
    assert_eq!(denied.event_type, "Denied");
    assert_eq!(denied.payload["reason"], "CapabilityDenied");
    assert_eq!(denied.payload["capability"], "verification.run");
}

#[test]
fn apply_patch_against_a_stale_workspace_is_a_version_conflict_with_audit() {
    let fx = setup();
    let actual = fx.base();
    let stale = Digest::of(b"stale");
    let (task, events) = (fx.task(), fx.events());
    let err = fx.patch(b"p", stale).unwrap_err();
    assert!(
        matches!(err, DbError::VersionConflict { expected, actual: a } if expected == stale && a == actual),
        "{err:?}"
    );
    assert_eq!(fx.task(), task);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
    let after = fx.reopen().events(&fx.id).unwrap();
    assert_eq!(after.len(), events.len() + 1);
    let denied = after.last().unwrap();
    assert_eq!(denied.event_type, "Denied");
    assert_eq!(denied.payload["reason"], "VersionConflict");
    assert_eq!(denied.payload["expected"], stale.to_string());
    assert_eq!(denied.payload["actual"], actual.to_string());
}

#[test]
fn apply_patch_whose_expected_base_disagrees_is_a_version_conflict() {
    let fx = setup();
    let base = fx.base();
    let kind = EffectKind::ApplyPatch { expected_base: Digest::of(b"other") };
    let r = Reservation::for_kind(&kind, 0);
    let err = fx.db.record_intent(&fx.id, kind, Digest::of(b"p"), &base, r, &src()).unwrap_err();
    assert!(matches!(err, DbError::VersionConflict { .. }), "{err:?}");
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
}

#[test]
fn non_patch_kinds_just_record_the_expected_workspace() {
    let fx = setup();
    let k = EffectKind::RunVerification;
    let e = fx
        .db
        .record_intent(&fx.id, k.clone(), Digest::of(b"v"), &Digest::of(b"whatever"), Reservation::for_kind(&k, 0), &profile())
        .unwrap();
    let stored: String = Connection::open(&fx.path)
        .unwrap()
        .query_row("SELECT expected_workspace FROM effects WHERE effect_id = ?1", [e.effect_id.as_str()], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, Digest::of(b"whatever").to_string());
}

#[test]
fn intent_is_refused_while_cancel_is_pending() {
    let fx = setup();
    fx.db.append(&fx.id, &TaskEvent::CancelRequested).unwrap();
    let (task, events) = (fx.task(), fx.events());
    let err = fx.verify(b"v", 1).unwrap_err();
    assert!(matches!(err, DbError::NotDispatchable { cancel_requested: true, .. }), "{err:?}");
    assert_eq!(fx.task(), task);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
}

#[test]
fn intent_is_refused_for_terminal_and_parked_tasks() {
    let fx = setup();
    fx.db.append(&fx.id, &TaskEvent::Paused).unwrap();
    assert!(matches!(fx.verify(b"v", 1), Err(DbError::NotDispatchable { state: TaskState::Paused, .. })));
    fx.db.append(&fx.id, &TaskEvent::Failed { reason: "x".into() }).unwrap();
    let events = fx.events();
    assert!(matches!(fx.read(b"r", 1), Err(DbError::NotDispatchable { state: TaskState::Failed, .. })));
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 0);
}

#[test]
fn action_consuming_intent_while_verifying_is_a_transition_error_not_budget() {
    let fx = setup();
    fx.db.append(&fx.id, &TaskEvent::VerifyStarted).unwrap();
    let err = fx.read(b"r", 0).unwrap_err();
    assert!(
        matches!(err, DbError::Transition(TransitionError::InvalidTransition { state: TaskState::Verifying, .. })),
        "{err:?}"
    );
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
    // Verification work is fine while verifying.
    fx.verify(b"v", 0).unwrap();
}

#[test]
fn record_intent_for_missing_task_is_not_found() {
    let fx = setup();
    let k = EffectKind::ReadSnapshot;
    let r = fx.db.record_intent(&TaskId::new(), k.clone(), Digest::of(b"r"), &fx.base(), Reservation::for_kind(&k, 0), &Resource::Task);
    assert!(matches!(r, Err(DbError::NotFound(_))));
}

// ---------- mark_dispatched ----------

#[test]
fn dispatch_moves_intended_to_dispatched_and_records_the_attempt() {
    let fx = setup();
    let e = fx.read(b"r", 1).unwrap();
    let a = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a, "worker-1", 1).unwrap();
    let d = fx.db.effect(&e.effect_id).unwrap();
    assert_eq!((d.state, d.lease_generation), (EffectState::Dispatched, 1));
    let (worker, lease): (String, i64) = Connection::open(&fx.path)
        .unwrap()
        .query_row(
            "SELECT worker, lease_generation FROM attempts WHERE attempt_id = ?1 AND effect_id = ?2 AND finished_ts IS NULL",
            [a.to_string(), e.effect_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((worker.as_str(), lease), ("worker-1", 1));
    let ev = fx.events().into_iter().last().unwrap();
    assert_eq!(ev.event_type, "EffectDispatched");
    assert_eq!(ev.payload["effect_id"], e.effect_id.to_string());
    assert_eq!(ev.payload["attempt_id"], a.to_string());
}

#[test]
fn redispatch_requires_a_strictly_newer_lease_and_closes_the_old_attempt() {
    let fx = setup();
    let (e, _a1) = fx.dispatched(b"r", 1);
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w2", 1).unwrap_err();
    assert!(matches!(err, DbError::StaleLease { stored: 1, got: 1 }), "{err:?}");
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w2", 0).unwrap_err();
    assert!(matches!(err, DbError::StaleLease { stored: 1, got: 0 }), "{err:?}");
    fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w2", 2).unwrap();
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().lease_generation, 2);
    assert_eq!(fx.count("SELECT count(*) FROM attempts"), 2);
    assert_eq!(fx.count("SELECT count(*) FROM attempts WHERE finished_ts IS NULL"), 1);
    assert_eq!(fx.count_events("EffectDispatched"), 2);
}

#[test]
fn dispatch_of_finished_effects_is_rejected() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    fx.succeed(&e, &a, 1, None).unwrap();
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 5).unwrap_err();
    assert!(
        matches!(err, DbError::InvalidEffectTransition { from: EffectState::Completed, to: EffectState::Dispatched, .. }),
        "{err:?}"
    );
    let (f, fa) = fx.dispatched(b"f", 1);
    fx.db.complete_effect(&f.effect_id, &receipt(&f, &fa, 1, Outcome::Failure("x".into())), None, None).unwrap();
    let err = fx.db.mark_dispatched(&f.effect_id, &AttemptId::new(), "w", 5).unwrap_err();
    assert!(matches!(err, DbError::InvalidEffectTransition { from: EffectState::Failed, .. }), "{err:?}");
}

#[test]
fn unknown_effect_redispatches_only_under_a_strictly_newer_lease() {
    let fx = setup();
    let (e, _a1) = fx.dispatched(b"r", 3);
    fx.db.mark_unknown(&e.effect_id).unwrap();
    for stale in [2, 3] {
        let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w2", stale).unwrap_err();
        assert!(matches!(err, DbError::StaleLease { stored: 3, got } if got == stale), "{err:?}");
    }
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Unknown);
    let a2 = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a2, "w2", 4).unwrap();
    let d = fx.db.effect(&e.effect_id).unwrap();
    assert_eq!((d.state, d.lease_generation), (EffectState::Dispatched, 4));
    assert_eq!(fx.count("SELECT count(*) FROM attempts WHERE finished_ts IS NULL"), 1);
    // The reservation stays uncertain: the first attempt may still have run.
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Uncertain'"), 1);
    assert_eq!(fx.usage().uncertain_model_requests, 1);
    // It can go unknown again and still settles exactly once.
    fx.db.mark_unknown(&e.effect_id).unwrap();
    let a3 = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a3, "w3", 5).unwrap();
    fx.succeed(&e, &a3, 5, None).unwrap();
    let u = fx.usage();
    assert_eq!((u.reserved_model_requests, u.uncertain_model_requests, u.settled_model_requests), (0, 0, 1));
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 1);
}

#[test]
fn dispatch_is_refused_while_paused_or_waiting() {
    for park in [TaskEvent::Paused, TaskEvent::Waiting] {
        let fx = setup();
        let e = fx.read(b"r", 1).unwrap();
        fx.db.append(&fx.id, &park).unwrap();
        let events = fx.events();
        let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap_err();
        assert!(matches!(err, DbError::NotDispatchable { cancel_requested: false, .. }), "{park:?} {err:?}");
        assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Intended);
        assert_eq!(fx.events(), events);
        assert_eq!(fx.count("SELECT count(*) FROM attempts"), 0);
    }
}

#[test]
fn dispatch_is_refused_once_cancel_is_requested() {
    let fx = setup();
    let e = fx.read(b"r", 1).unwrap();
    fx.db.append(&fx.id, &TaskEvent::CancelRequested).unwrap();
    let events = fx.events();
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap_err();
    assert!(matches!(err, DbError::NotDispatchable { cancel_requested: true, .. }), "{err:?}");
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Intended);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM attempts"), 0);
}

#[test]
fn unknown_effect_id_is_effect_not_found() {
    let fx = setup();
    let ghost = EffectId::derive(&fx.id, 99, &EffectKind::ReadSnapshot, &Digest::of(b"g"));
    assert!(matches!(fx.db.effect(&ghost), Err(DbError::EffectNotFound(_))));
    assert!(matches!(fx.db.mark_dispatched(&ghost, &AttemptId::new(), "w", 1), Err(DbError::EffectNotFound(_))));
    assert!(matches!(fx.db.mark_unknown(&ghost), Err(DbError::EffectNotFound(_))));
    let r = Receipt {
        effect_id: ghost.clone(),
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        outcome: Outcome::Success,
        result_digest: None,
    };
    assert!(matches!(fx.db.complete_effect(&ghost, &r, None, None), Err(DbError::EffectNotFound(_))));
}

// ---------- complete_effect ----------

#[test]
fn happy_path_apply_patch_completes_and_advances_the_workspace() {
    let fx = setup();
    let blobs = BlobStore::open(fx.path.parent().unwrap().join("blobs")).unwrap();
    let base = fx.base();
    let e = fx.patch(b"patch-1", base).unwrap();
    assert_eq!(fx.db.outstanding_effects(&fx.id).unwrap(), vec![e.clone()]);
    let a = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a, "worker-1", 1).unwrap();

    // Publish order: blob first, then register, then complete.
    let bytes = b"--- a/src/parser.py\n+++ b/src/parser.py\n";
    let artifact = blobs.put(bytes).unwrap();
    fx.db
        .register_artifact(&artifact, bytes.len() as u64, "patch", Some(&e.effect_id), "worker-1")
        .unwrap();
    let new_ws = Digest::of(b"workspace-after-patch");
    let r = Receipt { result_digest: Some(artifact), ..receipt(&e, &a, 1, Outcome::Success) };
    let v = fx
        .db
        .complete_effect(&e.effect_id, &r, Some(&artifact), Some(TaskEvent::WorkspaceUpdated { digest: new_ws }))
        .unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);

    let done = fx.db.effect(&e.effect_id).unwrap();
    // The effect references the published blob; the new workspace digest travels in the follow-up.
    assert_eq!((done.state, done.result_digest), (EffectState::Completed, Some(artifact)));
    assert_eq!(fx.task().workspace_digest, new_ws);
    assert_eq!(fx.task().verified_digest, None);
    let u = fx.usage();
    assert_eq!((u.reserved_model_requests, u.settled_model_requests), (0, 1));
    assert_eq!((u.reserved_tool_actions, u.settled_tool_actions), (0, 1));
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Settled'"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM attempts WHERE finished_ts IS NULL"), 0);
    assert!(fx.db.outstanding_effects(&fx.id).unwrap().is_empty());
    let types: Vec<String> = fx.events().iter().rev().take(2).map(|e| e.event_type.clone()).collect();
    assert_eq!(types, vec!["WorkspaceUpdated", "EffectCompleted"]);
    assert_eq!(blobs.get(&artifact).unwrap(), bytes);
}

#[test]
fn failure_outcome_marks_the_effect_failed_and_settles_usage() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let v = fx
        .db
        .complete_effect(&e.effect_id, &receipt(&e, &a, 1, Outcome::Failure("exit 2".into())), None, None)
        .unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Failed);
    assert_eq!(fx.usage().settled_model_requests, 1);
    let ev = fx.events().into_iter().last().unwrap();
    assert_eq!(ev.event_type, "EffectFailed");
    assert_eq!(ev.payload["outcome"], json!({"Failure": "exit 2"}));
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().result_digest, None);
}

#[test]
fn failure_may_reference_a_published_error_artifact() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let log = fx.publish(&e, b"stderr: boom");
    let r = receipt(&e, &a, 1, Outcome::Failure("exit 2".into()));
    assert_eq!(fx.db.complete_effect(&e.effect_id, &r, Some(&log), None).unwrap(), ReceiptVerdict::Apply);
    let f = fx.db.effect(&e.effect_id).unwrap();
    assert_eq!((f.state, f.result_digest), (EffectState::Failed, Some(log)));
}

#[test]
fn success_without_an_artifact_is_refused_and_nothing_changes() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let r = Receipt { result_digest: Some(Digest::of(b"never published")), ..receipt(&e, &a, 1, Outcome::Success) };
    for receipt in [r.clone(), Receipt { result_digest: None, ..r }] {
        let err = fx.db.complete_effect(&e.effect_id, &receipt, None, Some(TaskEvent::ActionUsed)).unwrap_err();
        assert!(matches!(err, DbError::ArtifactRequired(ref id) if *id == e.effect_id), "{err:?}");
    }
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.events(), events);
}

#[test]
fn artifact_registered_for_another_effect_or_none_is_refused() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let other = fx.read(b"other", 0).unwrap();
    let foreign = fx.publish(&other, b"other result");
    let loose = fx.blobs().put(b"loose").unwrap();
    fx.db.register_artifact(&loose, 5, "log", None, "test").unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let r = receipt(&e, &a, 1, Outcome::Success);
    let err = fx.db.complete_effect(&e.effect_id, &r, Some(&foreign), None).unwrap_err();
    assert!(
        matches!(&err, DbError::ArtifactEffectMismatch { artifact, effect }
            if *artifact == foreign && *effect == e.effect_id),
        "{err:?}"
    );
    let err = fx.db.complete_effect(&e.effect_id, &r, Some(&loose), None).unwrap_err();
    assert!(matches!(&err, DbError::ArtifactEffectMismatch { artifact, .. } if *artifact == loose), "{err:?}");
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.events(), events);
}

#[test]
fn two_effects_with_byte_identical_artifacts_both_complete() {
    let fx = setup();
    let (e1, a1) = fx.dispatched(b"1", 1);
    let (e2, a2) = fx.dispatched(b"2", 1);
    assert_eq!(fx.succeed(&e1, &a1, 1, None).unwrap(), ReceiptVerdict::Apply);
    assert_eq!(fx.succeed(&e2, &a2, 1, None).unwrap(), ReceiptVerdict::Apply);
    let d = Digest::of(SAME_OUTPUT);
    for e in [&e1, &e2] {
        let done = fx.db.effect(&e.effect_id).unwrap();
        assert_eq!((done.state, done.result_digest), (EffectState::Completed, Some(d)));
    }
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM artifact_links"), 2);
    assert_eq!(fx.count_events("ArtifactRegistered"), 2);
}

#[test]
fn loose_registration_then_an_effect_claiming_the_same_bytes_completes() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let d = fx.blobs().put(b"shared").unwrap();
    fx.db.register_artifact(&d, 6, "log", None, "import").unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM artifact_links"), 0);
    let r = Receipt { result_digest: Some(d), ..receipt(&e, &a, 1, Outcome::Success) };
    assert!(matches!(
        fx.db.complete_effect(&e.effect_id, &r, Some(&d), None),
        Err(DbError::ArtifactEffectMismatch { .. })
    ));
    fx.db.register_artifact(&d, 6, "log", Some(&e.effect_id), "worker-1").unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM artifact_links"), 1);
    assert_eq!(fx.count_events("ArtifactRegistered"), 1);
    assert_eq!(fx.db.complete_effect(&e.effect_id, &r, Some(&d), None).unwrap(), ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().result_digest, Some(d));
}

#[test]
fn effect_not_linked_to_identical_existing_content_is_refused() {
    let fx = setup();
    let (e1, a1) = fx.dispatched(b"1", 1);
    let (e2, a2) = fx.dispatched(b"2", 1);
    fx.succeed(&e1, &a1, 1, None).unwrap();
    // e2 produced the same bytes but never registered them for itself.
    let d = Digest::of(SAME_OUTPUT);
    let before = fx.snapshot(&e2.effect_id);
    let events = fx.events();
    let r = Receipt { result_digest: Some(d), ..receipt(&e2, &a2, 1, Outcome::Success) };
    let err = fx.db.complete_effect(&e2.effect_id, &r, Some(&d), None).unwrap_err();
    assert!(
        matches!(&err, DbError::ArtifactEffectMismatch { artifact, effect } if *artifact == d && *effect == e2.effect_id),
        "{err:?}"
    );
    assert_eq!(fx.snapshot(&e2.effect_id), before);
    assert_eq!(fx.events(), events);
}

#[test]
fn referenced_blobs_are_all_registered_artifacts() {
    let fx = setup();
    assert!(fx.db.referenced_blobs().unwrap().is_empty());
    let (e1, a1) = fx.dispatched(b"1", 1);
    let (e2, a2) = fx.dispatched(b"2", 1);
    fx.succeed(&e1, &a1, 1, None).unwrap();
    fx.succeed(&e2, &a2, 1, None).unwrap();
    let loose = fx.blobs().put(b"loose").unwrap();
    fx.db.register_artifact(&loose, 5, "log", None, "import").unwrap();
    let orphan = fx.blobs().put(b"never registered").unwrap();
    let refs = fx.reopen().referenced_blobs().unwrap();
    assert_eq!(refs, HashSet::from([Digest::of(SAME_OUTPUT), loose]));
    // Feeding it to gc keeps every registered blob and removes only the orphan.
    let blobs = fx.blobs();
    assert_eq!(blobs.gc(&refs).unwrap(), 1);
    assert!(blobs.exists(&loose) && blobs.exists(&Digest::of(SAME_OUTPUT)));
    assert!(!blobs.exists(&orphan));
}

#[test]
fn receipt_digest_disagreeing_with_the_artifact_is_refused() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let art = fx.publish(&e, b"result");
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let wrong = Digest::of(b"something else");
    let r = Receipt { result_digest: Some(wrong), ..receipt(&e, &a, 1, Outcome::Success) };
    let err = fx.db.complete_effect(&e.effect_id, &r, Some(&art), None).unwrap_err();
    assert!(
        matches!(err, DbError::ReceiptArtifactMismatch { artifact, receipt } if artifact == art && receipt == wrong),
        "{err:?}"
    );
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.events(), events);
    // A receipt that carries no digest defers to the registered artifact.
    let r = receipt(&e, &a, 1, Outcome::Success);
    assert_eq!(fx.db.complete_effect(&e.effect_id, &r, Some(&art), None).unwrap(), ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().result_digest, Some(art));
}

#[test]
fn receipt_with_a_newer_lease_than_stored_is_applied() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let v = fx.succeed(&e, &a, 7, None).unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Completed);
}

#[test]
fn duplicate_receipt_changes_nothing_and_adds_one_audit_event() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let art = fx.publish(&e, b"result");
    let r = Receipt { result_digest: Some(art), ..receipt(&e, &a, 1, Outcome::Success) };
    fx.db.complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::ActionUsed)).unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let v = fx.db.complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::ActionUsed)).unwrap();
    assert_eq!(v, ReceiptVerdict::DuplicateIgnored);
    assert_eq!(fx.snapshot(&e.effect_id), before);
    let after = fx.reopen().events(&fx.id).unwrap();
    assert_eq!(after.len(), events.len() + 1);
    assert_eq!(after[..events.len()], events[..]);
    let audit = after.last().unwrap();
    assert_eq!(audit.event_type, "ReceiptIgnored");
    assert_eq!(audit.payload["reason"], "DuplicateIgnored");
    assert_eq!(audit.payload["effect_id"], e.effect_id.to_string());
}

#[test]
fn stale_lease_receipt_changes_nothing_and_adds_one_audit_event() {
    let fx = setup();
    let (e, a1) = fx.dispatched(b"r", 1);
    fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "worker-2", 2).unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let v = fx.db.complete_effect(&e.effect_id, &receipt(&e, &a1, 1, Outcome::Success), None, None).unwrap();
    assert_eq!(v, ReceiptVerdict::StaleLeaseIgnored);
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Dispatched);
    let after = fx.reopen().events(&fx.id).unwrap();
    assert_eq!(after.len(), events.len() + 1);
    let audit = after.last().unwrap();
    assert_eq!(audit.event_type, "ReceiptIgnored");
    assert_eq!(audit.payload["reason"], "StaleLeaseIgnored");
}

#[test]
fn receipt_naming_another_effect_is_ignored_with_audit() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let other = fx.read(b"other", 0).unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let v = fx.db.complete_effect(&e.effect_id, &receipt(&other, &a, 1, Outcome::Success), None, None).unwrap();
    assert_eq!(v, ReceiptVerdict::WrongEffect);
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.db.effect(&other.effect_id).unwrap().state, EffectState::Intended);
    let after = fx.events();
    assert_eq!(after.len(), events.len() + 1);
    assert_eq!(after.last().unwrap().event_type, "ReceiptIgnored");
    assert_eq!(after.last().unwrap().payload["reason"], "WrongEffect");
}

#[test]
fn receipt_for_an_undispatched_effect_is_rejected_with_audit() {
    let fx = setup();
    let e = fx.read(b"r", 1).unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let v = fx
        .db
        .complete_effect(&e.effect_id, &receipt(&e, &AttemptId::new(), 0, Outcome::Success), None, None)
        .unwrap();
    assert_eq!(v, ReceiptVerdict::NotDispatched);
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Intended);
    let after = fx.reopen().events(&fx.id).unwrap();
    assert_eq!(after.len(), events.len() + 1);
    let audit = after.last().unwrap();
    assert_eq!(audit.event_type, "ReceiptRejected");
    assert_eq!(audit.payload["reason"], "NotDispatched");
}

#[test]
fn unregistered_artifact_is_refused_and_nothing_changes() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let blobs = BlobStore::open(fx.path.parent().unwrap().join("blobs")).unwrap();
    // Blob written but never registered: still not published.
    let artifact = blobs.put(b"orphan").unwrap();
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let r = receipt(&e, &a, 1, Outcome::Success);
    let err = fx
        .db
        .complete_effect(&e.effect_id, &r, Some(&artifact), Some(TaskEvent::ActionUsed))
        .unwrap_err();
    assert!(matches!(err, DbError::ArtifactNotPublished(d) if d == artifact), "{err:?}");
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.events(), events);

    // After registration the same receipt applies.
    fx.db.register_artifact(&artifact, 6, "log", Some(&e.effect_id), "worker-1").unwrap();
    assert_eq!(fx.db.complete_effect(&e.effect_id, &r, Some(&artifact), None).unwrap(), ReceiptVerdict::Apply);
}

#[test]
fn register_artifact_is_insert_or_ignore() {
    let fx = setup();
    let d = Digest::of(b"x");
    fx.db.register_artifact(&d, 1, "log", None, "test").unwrap();
    fx.db.register_artifact(&d, 1, "log", None, "test").unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
    assert_eq!(fx.count_events("ArtifactRegistered"), 0);
}

#[test]
fn artifact_linked_to_an_effect_is_journaled_once() {
    let fx = setup();
    let e = fx.read(b"r", 0).unwrap();
    let d = Digest::of(b"y");
    fx.db.register_artifact(&d, 1, "log", Some(&e.effect_id), "worker-1").unwrap();
    fx.db.register_artifact(&d, 1, "log", Some(&e.effect_id), "worker-1").unwrap();
    assert_eq!(fx.count_events("ArtifactRegistered"), 1);
    let ev = fx.events().into_iter().last().unwrap();
    assert_eq!(ev.payload["digest"], d.to_string());
    assert_eq!(ev.payload["effect_id"], e.effect_id.to_string());
    let ghost = EffectId::derive(&fx.id, 99, &EffectKind::ReadSnapshot, &Digest::of(b"g"));
    assert!(matches!(
        fx.db.register_artifact(&Digest::of(b"z"), 1, "log", Some(&ghost), "w"),
        Err(DbError::EffectNotFound(_))
    ));
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
}

#[test]
fn follow_up_rejected_by_pending_cancel_still_completes_the_effect() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    fx.db.append(&fx.id, &TaskEvent::CancelRequested).unwrap();
    let task_before = fx.task();
    let new_ws = Digest::of(b"w-new");
    let v = fx.succeed(&e, &a, 1, Some(TaskEvent::WorkspaceUpdated { digest: new_ws })).unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Completed);
    assert_eq!(fx.usage().settled_model_requests, 1);
    assert_eq!(fx.task(), task_before);
    let evs = fx.events();
    let types: Vec<&str> = evs.iter().rev().take(2).map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, vec!["TaskEventRejected", "EffectCompleted"]);
    let rejected = evs.last().unwrap();
    assert_eq!(rejected.payload["event"], json!({"WorkspaceUpdated": {"digest": new_ws.to_string()}}));
    assert!(rejected.payload["reason"].as_str().unwrap().contains("cancellation requested"));
}

#[test]
fn follow_up_rejected_on_a_terminal_task_still_completes_the_effect() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    fx.db.append(&fx.id, &TaskEvent::Failed { reason: "deadline".into() }).unwrap();
    let v = fx.succeed(&e, &a, 1, Some(TaskEvent::ActionUsed)).unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Completed);
    assert_eq!(fx.task().state, TaskState::Failed);
    assert_eq!(fx.events().last().unwrap().event_type, "TaskEventRejected");
}

#[test]
fn invalid_follow_up_rolls_back_the_whole_completion() {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let art = fx.publish(&e, b"result");
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let r = Receipt { result_digest: Some(art), ..receipt(&e, &a, 1, Outcome::Success) };
    // Started is not valid in Running.
    let err = fx.db.complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::Started)).unwrap_err();
    assert!(
        matches!(err, DbError::Transition(TransitionError::InvalidTransition { state: TaskState::Running, .. })),
        "{err:?}"
    );
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Dispatched);
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Reserved'"), 1);
    assert_eq!(fx.reopen().events(&fx.id).unwrap(), events);
    assert_eq!(fx.count_events("EffectCompleted"), 0);
}

#[test]
fn digest_mismatched_verify_follow_up_rolls_back_the_whole_completion() {
    let fx = setup();
    fx.db.append(&fx.id, &TaskEvent::VerifyStarted).unwrap();
    let e = fx.verify(fx.base().as_bytes(), 1).unwrap();
    let a = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a, "worker-1", 1).unwrap();
    let art = fx.publish(&e, b"evidence");
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    let r = Receipt { result_digest: Some(art), ..receipt(&e, &a, 1, Outcome::Success) };
    let wrong = Digest::of(b"not the workspace");
    let err = fx
        .db
        .complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::VerifyPassed { digest: wrong }))
        .unwrap_err();
    assert!(matches!(err, DbError::Transition(TransitionError::DigestMismatch { .. })), "{err:?}");
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.task().state, TaskState::Verifying);
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Reserved'"), 1);
    assert_eq!(fx.reopen().events(&fx.id).unwrap(), events);
    // The correct evidence then completes the effect and the task.
    let ws = fx.base();
    let v = fx.db.complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::VerifyPassed { digest: ws })).unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    assert_eq!(fx.task().state, TaskState::Succeeded);
}

// ---------- unknown ----------

#[test]
fn unknown_effect_keeps_its_reservation_and_a_late_receipt_settles_it() {
    let fx = setup_with(ALL_CAPS, 3, 100);
    let (e, a) = fx.dispatched(b"r", 1);
    fx.db.mark_unknown(&e.effect_id).unwrap();
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Unknown);
    assert_eq!(fx.events().last().unwrap().event_type, "EffectUnknown");
    let u = fx.usage();
    assert_eq!(
        (u.reserved_model_requests, u.uncertain_model_requests, u.settled_model_requests),
        (0, 1, 0)
    );
    assert_eq!(u.uncertain_tool_actions, 1);
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Uncertain'"), 1);
    assert_eq!(fx.db.outstanding_effects(&fx.id).unwrap()[0].state, EffectState::Unknown);
    // The uncertain request still counts against the limit.
    assert!(matches!(fx.read(b"x", 3), Err(DbError::BudgetExceeded(_))));
    // Survives a reopen.
    assert_eq!(fx.reopen().usage_summary(&fx.id).unwrap(), u);

    let v = fx.succeed(&e, &a, 1, None).unwrap();
    assert_eq!(v, ReceiptVerdict::Apply);
    let u = fx.usage();
    assert_eq!(
        (u.reserved_model_requests, u.uncertain_model_requests, u.settled_model_requests),
        (0, 0, 1)
    );
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Completed);
}

#[test]
fn mark_unknown_is_only_valid_from_dispatched() {
    let fx = setup();
    let e = fx.read(b"r", 1).unwrap();
    let err = fx.db.mark_unknown(&e.effect_id).unwrap_err();
    assert!(matches!(err, DbError::InvalidEffectTransition { from: EffectState::Intended, to: EffectState::Unknown, .. }));
    let (d, a) = fx.dispatched(b"d", 1);
    fx.db.mark_unknown(&d.effect_id).unwrap();
    assert!(matches!(fx.db.mark_unknown(&d.effect_id), Err(DbError::InvalidEffectTransition { from: EffectState::Unknown, .. })));
    fx.succeed(&d, &a, 1, None).unwrap();
    assert!(matches!(fx.db.mark_unknown(&d.effect_id), Err(DbError::InvalidEffectTransition { from: EffectState::Completed, .. })));
    assert_eq!(fx.count_events("EffectUnknown"), 1);
}

// ---------- reads ----------

#[test]
fn outstanding_effects_are_in_creation_order_and_exclude_finished() {
    let fx = setup();
    let e1 = fx.read(b"1", 0).unwrap();
    let (e2, a2) = fx.dispatched(b"2", 1);
    let e3 = fx.verify(b"3", 0).unwrap();
    let (e4, _) = fx.dispatched(b"4", 1);
    fx.db.mark_unknown(&e4.effect_id).unwrap();
    let ids: Vec<EffectId> = fx.db.outstanding_effects(&fx.id).unwrap().into_iter().map(|e| e.effect_id).collect();
    assert_eq!(ids, vec![e1.effect_id.clone(), e2.effect_id.clone(), e3.effect_id.clone(), e4.effect_id.clone()]);
    fx.succeed(&e2, &a2, 1, None).unwrap();
    let out = fx.db.outstanding_effects(&fx.id).unwrap();
    let got: Vec<(EffectId, EffectState)> = out.into_iter().map(|e| (e.effect_id, e.state)).collect();
    assert_eq!(
        got,
        vec![
            (e1.effect_id, EffectState::Intended),
            (e3.effect_id, EffectState::Intended),
            (e4.effect_id, EffectState::Unknown)
        ]
    );
}

#[test]
fn outstanding_effects_are_scoped_to_their_task() {
    let fx = setup();
    fx.read(b"1", 0).unwrap();
    let (c, d) = contract(ALL_CAPS, 10, 100);
    let other = fx.db.create_task(&c, &d).unwrap();
    fx.db.approve_task(&other).unwrap();
    assert!(fx.db.outstanding_effects(&other).unwrap().is_empty());
    assert_eq!(fx.db.usage_summary(&other).unwrap(), UsageSummary::default());
}

// ---------- atomicity ----------

#[test]
fn failed_completion_event_rolls_back_effect_usage_and_task() {
    completion_fault_rolls_back_everything("EffectCompleted");
}

#[test]
fn failed_follow_up_event_rolls_back_effect_usage_and_task() {
    // Catches committing the effect before applying the follow-up.
    completion_fault_rolls_back_everything("WorkspaceUpdated");
}

fn completion_fault_rolls_back_everything(failing_event: &str) {
    let fx = setup();
    let (e, a) = fx.dispatched(b"r", 1);
    let art = fx.publish(&e, b"result");
    let before = fx.snapshot(&e.effect_id);
    let events = fx.events();
    inject_failure(&fx.path, failing_event);

    let r = Receipt { result_digest: Some(art), ..receipt(&e, &a, 1, Outcome::Success) };
    let err = fx
        .db
        .complete_effect(&e.effect_id, &r, Some(&art), Some(TaskEvent::WorkspaceUpdated { digest: Digest::of(b"w") }))
        .unwrap_err();
    assert!(matches!(err, DbError::Sqlite(_)), "{err:?}");
    assert_eq!(fx.snapshot(&e.effect_id), before);
    assert_eq!(fx.events(), events);

    let db = fx.reopen();
    assert_eq!(db.task(&fx.id).unwrap(), before.0);
    assert_eq!(db.effect(&e.effect_id).unwrap(), before.1);
    assert_eq!(db.effect(&e.effect_id).unwrap().state, EffectState::Dispatched);
    assert_eq!(db.usage_summary(&fx.id).unwrap(), before.2);
    assert_eq!(fx.count("SELECT count(*) FROM usage WHERE status = 'Reserved'"), 1);
    assert_eq!(db.outstanding_effects(&fx.id).unwrap(), vec![before.1.clone()]);
    assert_eq!(db.events(&fx.id).unwrap(), events);
}

#[test]
fn failed_intent_event_rolls_back_effect_usage_and_action() {
    intent_fault_rolls_back_everything("EffectIntended");
}

#[test]
fn failed_action_used_event_rolls_back_effect_and_usage() {
    // Catches committing after EffectIntended and consuming the action in a second transaction.
    intent_fault_rolls_back_everything("ActionUsed");
}

fn intent_fault_rolls_back_everything(failing_event: &str) {
    let fx = setup();
    let (task, events) = (fx.task(), fx.events());
    inject_failure(&fx.path, failing_event);
    assert!(matches!(fx.read(b"r", 1), Err(DbError::Sqlite(_))));
    let db = fx.reopen();
    assert_eq!(db.task(&fx.id).unwrap(), task);
    assert_eq!(db.events(&fx.id).unwrap(), events);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM usage"), 0);
}

#[test]
fn reopened_database_shows_identical_effects_and_usage() {
    let fx = setup();
    let (e1, a1) = fx.dispatched(b"1", 1);
    fx.succeed(&e1, &a1, 1, None).unwrap();
    let (e2, _) = fx.dispatched(b"2", 3);
    fx.db.mark_unknown(&e2.effect_id).unwrap();
    fx.verify(b"3", 2).unwrap();
    let effects = [fx.db.effect(&e1.effect_id).unwrap(), fx.db.effect(&e2.effect_id).unwrap()];
    let (outstanding, usage, events) = (fx.db.outstanding_effects(&fx.id).unwrap(), fx.usage(), fx.events());
    let db = fx.reopen();
    assert_eq!([db.effect(&e1.effect_id).unwrap(), db.effect(&e2.effect_id).unwrap()], effects);
    assert_eq!(db.outstanding_effects(&fx.id).unwrap(), outstanding);
    assert_eq!(db.usage_summary(&fx.id).unwrap(), usage);
    assert_eq!(db.events(&fx.id).unwrap(), events);
    assert_eq!(
        usage,
        UsageSummary {
            reserved_model_requests: 2,
            settled_model_requests: 1,
            uncertain_model_requests: 1,
            reserved_tool_actions: 0,
            settled_tool_actions: 1,
            uncertain_tool_actions: 1,
        }
    );
    assert_eq!(effects[1].lease_generation, 3);
}

#[test]
fn abandoning_is_refused_while_the_task_could_still_dispatch() {
    let fx = setup();
    let e = fx.read(b"r", 1).unwrap();
    let before = fx.snapshot(&e.effect_id);
    let err = fx.db.abandon_effect(&e.effect_id, "test").unwrap_err();
    assert!(matches!(err, DbError::NotAbandonable { state: TaskState::Running, cancel_requested: false }), "{err:?}");
    assert!(fx.db.abandon_outstanding(&fx.id).is_err());
    fx.db.append(&fx.id, &TaskEvent::Paused).unwrap();
    // A paused task may resume and dispatch the effect after all.
    assert!(matches!(fx.db.abandon_effect(&e.effect_id, "test"), Err(DbError::NotAbandonable { .. })));
    assert_eq!(fx.db.effect(&e.effect_id).unwrap(), before.1);
    assert_eq!(fx.usage(), before.2);
    assert_eq!(fx.count_events("EffectAbandoned"), 0);
}

#[test]
fn abandoning_intended_effects_of_a_cancelled_task_releases_their_reservations() {
    let fx = setup();
    let (done, a) = fx.dispatched(b"done", 1);
    fx.succeed(&done, &a, 1, None).unwrap();
    let e1 = fx.read(b"1", 2).unwrap();
    let e2 = fx.verify(b"2", 3).unwrap();
    let (in_flight, _) = fx.dispatched(b"3", 1);
    let before = fx.usage();
    assert_eq!((before.reserved_model_requests, before.reserved_tool_actions), (2 + 3 + 1, 2));

    fx.db.append(&fx.id, &TaskEvent::CancelRequested).unwrap();
    fx.db.append(&fx.id, &TaskEvent::CancelCompleted).unwrap();
    let abandoned = fx.db.abandon_outstanding(&fx.id).unwrap();

    assert_eq!(abandoned, vec![e1.effect_id.clone(), e2.effect_id.clone()], "only never-dispatched effects");
    for e in [&e1, &e2] {
        assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Abandoned);
    }
    assert_eq!(fx.db.effect(&in_flight.effect_id).unwrap().state, EffectState::Dispatched);
    let usage = fx.usage();
    assert_eq!((usage.reserved_model_requests, usage.reserved_tool_actions), (1, 1), "only the in-flight one");
    assert_eq!((usage.settled_model_requests, usage.settled_tool_actions), (before.settled_model_requests, 1));
    assert_eq!((usage.uncertain_model_requests, usage.uncertain_tool_actions), (0, 0));
    let events: Vec<_> = fx.events().into_iter().filter(|e| e.event_type == "EffectAbandoned").collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].payload["previous_state"], "Intended");
    assert_eq!(fx.db.outstanding_effects(&fx.id).unwrap().len(), 1);

    // Idempotent: nothing left to abandon, nothing journaled.
    let n = fx.events().len();
    assert!(fx.db.abandon_outstanding(&fx.id).unwrap().is_empty());
    assert_eq!(fx.events().len(), n);
    // A receipt for an abandoned effect is rejected and changes nothing.
    let r = receipt(&e1, &AttemptId::new(), 1, Outcome::Success);
    assert_eq!(fx.db.complete_effect(&e1.effect_id, &r, None, None).unwrap(), ReceiptVerdict::NotDispatched);
    assert_eq!(fx.db.effect(&e1.effect_id).unwrap().state, EffectState::Abandoned);
    assert_eq!(fx.usage(), usage);
}

#[test]
fn a_dispatched_effect_proven_not_applied_may_be_abandoned_once_cancel_is_pending() {
    let fx = setup();
    let (e, _) = fx.dispatched(b"r", 1);
    fx.db.mark_unknown(&e.effect_id).unwrap();
    assert_eq!(fx.usage().uncertain_tool_actions, 1);
    fx.db.append(&fx.id, &TaskEvent::CancelRequested).unwrap();

    fx.db.abandon_effect(&e.effect_id, "reconciled: not applied").unwrap();

    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Abandoned);
    assert_eq!(fx.usage(), UsageSummary::default());
    assert_eq!(fx.count("SELECT count(*) FROM attempts WHERE finished_ts IS NULL"), 0);
    let ev = fx.events().into_iter().find(|e| e.event_type == "EffectAbandoned").unwrap();
    assert_eq!(ev.payload["previous_state"], "Unknown");
    assert_eq!(ev.payload["reason"], "reconciled: not applied");
    // Finished effects cannot be abandoned.
    let err = fx.db.abandon_effect(&e.effect_id, "again").unwrap_err();
    assert!(matches!(err, DbError::InvalidEffectTransition { from: EffectState::Abandoned, .. }), "{err:?}");
    // Survives a reopen.
    assert_eq!(fx.reopen().effect(&e.effect_id).unwrap().state, EffectState::Abandoned);
}

/// A verification effect intended, dispatched and published, ready to complete.
fn verification_ready(fx: &Fx, request: &[u8]) -> (EffectRecord, Receipt, Digest) {
    let e = fx.verify(request, 0).unwrap();
    let a = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a, "worker-1", 1).unwrap();
    let art = fx.publish(&e, b"evidence");
    (e.clone(), Receipt { result_digest: Some(art), ..receipt(&e, &a, 1, Outcome::Success) }, art)
}

#[test]
fn verify_passed_follow_up_needs_a_successful_verification_of_this_workspace() {
    let fx = setup();
    let ws = fx.base();
    let passed = || Some(TaskEvent::VerifyPassed { digest: ws });

    // Not a verification at all.
    let (snap, a) = fx.dispatched(b"s", 1);
    let art = fx.publish(&snap, b"manifest");
    let r = Receipt { result_digest: Some(art), ..receipt(&snap, &a, 1, Outcome::Success) };
    let before = (fx.snapshot(&snap.effect_id), fx.events());
    let err = fx.db.complete_effect(&snap.effect_id, &r, Some(&art), passed()).unwrap_err();
    assert!(matches!(err, DbError::UnprovenVerification(_)), "{err:?}");
    assert_eq!((fx.snapshot(&snap.effect_id), fx.events()), before, "nothing written");

    fx.db.append(&fx.id, &TaskEvent::VerifyStarted).unwrap();
    // A verification whose request is not bound to the current workspace.
    let (unbound, r, art) = verification_ready(&fx, b"some other request");
    let before = (fx.snapshot(&unbound.effect_id), fx.events());
    let err = fx.db.complete_effect(&unbound.effect_id, &r, Some(&art), passed()).unwrap_err();
    assert!(matches!(err, DbError::UnprovenVerification(_)), "{err:?}");
    assert_eq!((fx.snapshot(&unbound.effect_id), fx.events()), before);

    // A bound verification that failed.
    let (bound, r, art) = verification_ready(&fx, ws.as_bytes());
    let failed = Receipt { outcome: Outcome::Failure("timeout".into()), ..r.clone() };
    let err = fx.db.complete_effect(&bound.effect_id, &failed, Some(&art), passed()).unwrap_err();
    assert!(matches!(err, DbError::UnprovenVerification(_)), "{err:?}");
    assert_eq!(fx.task().state, TaskState::Verifying);

    // The bound, successful verification is accepted.
    assert_eq!(fx.db.complete_effect(&bound.effect_id, &r, Some(&art), passed()).unwrap(), ReceiptVerdict::Apply);
    assert_eq!(fx.task().state, TaskState::Succeeded);
    assert_eq!(fx.task().verified_digest, Some(ws));
}

#[test]
fn forfeited_model_request_stays_counted_as_uncertain() {
    let fx = setup_with(MODEL_CAPS, 2, 100);
    let one = |turn: u32, req: &[u8]| fx.db.record_intent(&fx.id, call(turn), Digest::of(req), &fx.base(), Reservation::for_kind(&call(turn), 1), &Resource::Task);
    let a = one(1, b"req-1").unwrap();
    let attempt = AttemptId::new();
    fx.db.mark_dispatched(&a.effect_id, &attempt, "model", 1).unwrap();
    fx.db.forfeit_effect(&a.effect_id, "transport failure: timed out").unwrap();
    let a2 = fx.db.effect(&a.effect_id).unwrap();
    assert_eq!((a2.state, a2.result_digest, a2.lease_generation), (EffectState::Failed, None, 1));
    let u = fx.usage();
    assert_eq!((u.reserved_model_requests, u.settled_model_requests, u.uncertain_model_requests), (0, 0, 1));
    assert!(fx.db.outstanding_effects(&fx.id).unwrap().is_empty());
    assert_eq!(fx.count_events("EffectForfeited"), 1);
    let ev = fx.events().into_iter().find(|e| e.event_type == "EffectForfeited").unwrap();
    assert_eq!(ev.payload["previous_state"], json!("Dispatched"));
    assert_eq!(ev.payload["reason"], "transport failure: timed out");
    assert_eq!(fx.count("SELECT COUNT(*) FROM attempts WHERE finished_ts IS NULL"), 0);
    // The task goes on: a second call settles as a failure (a 4xx), one reservation each.
    let b = one(2, b"req-2").unwrap();
    let attempt_b = AttemptId::new();
    fx.db.mark_dispatched(&b.effect_id, &attempt_b, "model", 1).unwrap();
    let r = receipt(&b, &attempt_b, 1, Outcome::Failure("http 400: bad request".into()));
    assert_eq!(fx.db.complete_effect(&b.effect_id, &r, None, None).unwrap(), ReceiptVerdict::Apply);
    assert_eq!(fx.usage().model_totals().committed(), 2);
    // Limit 2: uncertain + settled fill it; a third is refused and journals nothing but the refusal.
    let err = one(3, b"req-3").unwrap_err();
    assert!(matches!(err, DbError::BudgetExceeded(BudgetError::ModelRequests { limit: 2, committed: 2, requested: 1 })), "{err}");
    // A late receipt for the forfeited call can never be applied.
    let late = receipt(&a, &attempt, 1, Outcome::Success);
    assert_eq!(fx.db.complete_effect(&a.effect_id, &late, None, None).unwrap(), ReceiptVerdict::DuplicateIgnored);
    assert_eq!(fx.usage().uncertain_model_requests, 1);
}

#[test]
fn forfeit_is_only_for_dispatched_or_unknown_effects_and_its_event_is_reserved() {
    let fx = setup_with(MODEL_CAPS, 5, 100);
    let a = fx.db.record_intent(&fx.id, call(1), Digest::of(b"r"), &fx.base(), Reservation::for_kind(&call(1), 1), &Resource::Task).unwrap();
    assert!(matches!(fx.db.forfeit_effect(&a.effect_id, "x"), Err(DbError::InvalidEffectTransition { from: EffectState::Intended, to: EffectState::Failed, .. })));
    let attempt = AttemptId::new();
    fx.db.mark_dispatched(&a.effect_id, &attempt, "model", 1).unwrap();
    fx.db.mark_unknown(&a.effect_id).unwrap();
    fx.db.forfeit_effect(&a.effect_id, "from unknown").unwrap();
    assert_eq!(fx.db.effect(&a.effect_id).unwrap().state, EffectState::Failed);
    assert!(matches!(fx.db.forfeit_effect(&a.effect_id, "twice"), Err(DbError::InvalidEffectTransition { from: EffectState::Failed, .. })));
    assert!(matches!(fx.db.append_audit(&fx.id, "EffectForfeited", &json!({})), Err(DbError::ReservedEventType(_))));
}
