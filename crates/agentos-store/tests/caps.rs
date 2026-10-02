//! The capability broker's persistence: approval issues handles, every authorization taken
//! inside `record_intent` and `mark_dispatched` is journaled (prefixes only), `check` is pure,
//! revocation and expiry are enforced, and the schema version gate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use agentos_core::broker::{scope_for, Resource};
use agentos_core::budget::Reservation;
use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{AttemptId, EffectKind, EffectRecord, EffectState, Outcome, Receipt};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{Task, TaskEvent};
use agentos_store::db::{Db, DbError, StoredEvent, SCHEMA_VERSION};
use agentos_store::effects::UsageSummary;
use rusqlite::Connection;
use serde_json::{json, Value};

const ALL_CAPS: &[&str] = &["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"];
const T0: i64 = 1_700_000_000;
const DEADLINE: i64 = 600;

fn contract(caps: &[&str]) -> (Contract, Digest) {
    let json = json!({
        "goal": "fix the parser",
        "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
        "profile": "protected",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": caps,
        "limits": {
            "model_requests": 10,
            "max_output_tokens_per_request": 1000,
            "tool_actions": 100,
            "deadline_seconds": DEADLINE,
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }
    })
    .to_string();
    (Contract::parse(&json).unwrap(), Digest::of(json.as_bytes()))
}

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: Db,
    id: TaskId,
    contract: Contract,
    clock: Arc<AtomicI64>,
}

fn open_with_clock(path: &Path, clock: &Arc<AtomicI64>) -> Db {
    let c = clock.clone();
    Db::open(path).unwrap().with_clock(Box::new(move || c.load(Ordering::SeqCst)))
}

/// A started task that is NOT approved yet.
fn unapproved(caps: &[&str]) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let clock = Arc::new(AtomicI64::new(T0));
    let db = open_with_clock(&path, &clock);
    let (c, d) = contract(caps);
    let id = db.create_task(&c, &d).unwrap();
    db.append(&id, &TaskEvent::Started).unwrap();
    Fx { _dir: dir, path, db, id, contract: c, clock }
}

fn approved(caps: &[&str]) -> Fx {
    let fx = unapproved(caps);
    fx.db.approve_task(&fx.id).unwrap();
    fx
}

impl Fx {
    fn set_clock(&self, t: i64) {
        self.clock.store(t, Ordering::SeqCst);
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
    fn of_type(&self, t: &str) -> Vec<Value> {
        self.events().into_iter().filter(|e| e.event_type == t).map(|e| e.payload).collect()
    }
    fn usage(&self) -> UsageSummary {
        self.db.usage_summary(&self.id).unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        Connection::open(&self.path).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn read(&self) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::ReadSnapshot;
        let r = Reservation::for_kind(&kind, 0);
        self.db.record_intent(&self.id, kind, Digest::of(b"read"), &self.base(), r, &Resource::Task)
    }
    fn verify(&self) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::RunVerification;
        let r = Reservation::for_kind(&kind, 0);
        let profile = Resource::Profile(self.contract.verification_profile.clone());
        self.db.record_intent(&self.id, kind, Digest::of(b"verify"), &self.base(), r, &profile)
    }
    fn patch(&self, paths: &[&str]) -> Result<EffectRecord, DbError> {
        let base = self.base();
        let kind = EffectKind::ApplyPatch { expected_base: base };
        let r = Reservation::for_kind(&kind, 0);
        let resource = Resource::Paths(paths.iter().map(|p| p.to_string()).collect());
        self.db.record_intent(&self.id, kind, Digest::of(paths.join(",").as_bytes()), &base, r, &resource)
    }
    fn export(&self) -> Result<EffectRecord, DbError> {
        let kind = EffectKind::ExportBundle;
        let r = Reservation::for_kind(&kind, 0);
        self.db.record_intent(&self.id, kind, Digest::of(b"export"), &self.base(), r, &Resource::Task)
    }
    fn prefix(&self, op: Capability) -> String {
        let g = self.db.grants(&self.id).unwrap().into_iter().find(|g| g.operation == op).unwrap();
        g.handle.prefix().to_string()
    }
    /// Full handles, straight from the store's private column.
    fn raw_handles(&self) -> Vec<String> {
        let conn = Connection::open(&self.path).unwrap();
        let mut stmt = conn.prepare("SELECT id FROM capabilities").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }
    /// Everything a refused intent must leave untouched.
    fn state(&self) -> (Task, i64, i64, UsageSummary) {
        (self.task(), self.count("SELECT count(*) FROM effects"), self.count("SELECT count(*) FROM usage"), self.usage())
    }
}

fn denied(err: &DbError) -> (Capability, String) {
    match err {
        DbError::CapabilityDenied { capability, reason } => (*capability, reason.clone()),
        other => panic!("expected CapabilityDenied, got {other:?}"),
    }
}

fn op_name(op: Capability) -> Value {
    serde_json::to_value(op).unwrap()
}

// ---------- approval ----------

#[test]
fn approve_issues_one_handle_per_contract_capability_and_journals_prefixes_only() {
    let fx = unapproved(ALL_CAPS);
    assert!(fx.db.grants(&fx.id).unwrap().is_empty());
    let ops = fx.db.approve_task(&fx.id).unwrap();
    let want = vec![
        Capability::SnapshotRead,
        Capability::WorkspaceApplyPatch,
        Capability::VerificationRun,
        Capability::ArtifactExport,
    ];
    assert_eq!(ops, want);

    let grants = fx.db.grants(&fx.id).unwrap();
    assert_eq!(grants.iter().map(|g| g.operation).collect::<Vec<_>>(), want);
    let deadline = T0 + DEADLINE;
    for g in &grants {
        assert_eq!(g.task, fx.id);
        assert_eq!(g.scope, scope_for(g.operation, &fx.contract));
        assert!(!g.revoked);
        let expiry = if g.operation == Capability::ArtifactExport { None } else { Some(deadline) };
        assert_eq!(g.expires_ts, expiry, "{:?}", g.operation);
    }
    let handles: Vec<String> = grants.iter().map(|g| g.handle.to_string()).collect();
    assert_eq!(handles.iter().collect::<std::collections::HashSet<_>>().len(), 4, "distinct handles");
    assert_eq!(fx.count("SELECT count(*) FROM capabilities"), 4);

    let issued = fx.of_type("CapabilitiesIssued");
    assert_eq!(issued.len(), 1);
    let p = &issued[0];
    assert_eq!(p["operations"], json!(["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"]));
    let listed = p["handles"].as_array().unwrap();
    assert_eq!(listed.len(), 4);
    for (entry, g) in listed.iter().zip(&grants) {
        assert_eq!(entry["operation"], op_name(g.operation));
        assert_eq!(entry["prefix"], g.handle.prefix());
        assert_eq!(entry["prefix"].as_str().unwrap().len(), 8);
    }
    let text = p.to_string();
    for h in &handles {
        assert!(!text.contains(h.as_str()), "full handle in CapabilitiesIssued: {text}");
    }
}

#[test]
fn approve_sets_the_deadline_once() {
    let fx = unapproved(ALL_CAPS);
    assert_eq!(fx.db.deadline_ts(&fx.id).unwrap(), 0, "not started");
    fx.set_clock(i64::MAX / 2);
    assert!(!fx.db.deadline_passed(&fx.id).unwrap(), "0 is never passed");

    fx.set_clock(T0);
    fx.db.approve_task(&fx.id).unwrap();
    assert_eq!(fx.db.deadline_ts(&fx.id).unwrap(), T0 + DEADLINE);
    fx.set_clock(T0 + 100);
    fx.db.approve_task(&fx.id).unwrap();
    assert_eq!(fx.db.deadline_ts(&fx.id).unwrap(), T0 + DEADLINE, "a second approval keeps the first deadline");

    fx.set_clock(T0 + DEADLINE - 1);
    assert!(!fx.db.deadline_passed(&fx.id).unwrap());
    fx.set_clock(T0 + DEADLINE);
    assert!(fx.db.deadline_passed(&fx.id).unwrap());
    assert!(matches!(fx.db.deadline_ts(&TaskId::new()), Err(DbError::NotFound(_))));
}

#[test]
fn approve_is_idempotent() {
    let fx = unapproved(ALL_CAPS);
    let first = fx.db.approve_task(&fx.id).unwrap();
    let grants = fx.db.grants(&fx.id).unwrap();
    let (task, events) = (fx.task(), fx.events());
    fx.set_clock(T0 + 5);
    let second = fx.db.approve_task(&fx.id).unwrap();
    assert_eq!(second, first);
    assert_eq!(fx.db.grants(&fx.id).unwrap(), grants, "same handles");
    assert_eq!(fx.events(), events, "nothing journaled");
    assert_eq!(fx.task(), task);
    assert_eq!(fx.count("SELECT count(*) FROM capabilities"), 4);
    assert!(matches!(fx.db.approve_task(&TaskId::new()), Err(DbError::NotFound(_))));
}

#[test]
fn no_full_handle_appears_in_any_journal_payload() {
    let fx = approved(ALL_CAPS);
    // Granted at intent and dispatch, denied out of scope, revoked, denied at dispatch.
    let e = fx.read().unwrap();
    fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap();
    let denial = fx.patch(&["tests/x.py"]).unwrap_err();
    let v = fx.verify().unwrap();
    fx.db.revoke(&fx.id, Some(Capability::VerificationRun)).unwrap();
    let dispatch_denial = fx.db.mark_dispatched(&v.effect_id, &AttemptId::new(), "w", 1).unwrap_err();
    fx.db.revoke(&fx.id, None).unwrap();

    let handles = fx.raw_handles();
    assert_eq!(handles.len(), 4);
    let conn = Connection::open(&fx.path).unwrap();
    let mut stmt = conn.prepare("SELECT type, payload FROM events").unwrap();
    let rows: Vec<(String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    for kind in ["CapabilitiesIssued", "CapabilityGranted", "CapabilityDenied", "CapabilityRevoked"] {
        assert!(rows.iter().any(|(t, _)| t == kind), "the scenario journals {kind}");
    }
    for (t, payload) in &rows {
        for h in &handles {
            assert!(!payload.contains(h.as_str()) && !t.contains(h.as_str()), "full handle in {t}: {payload}");
        }
    }
    for err in [denial, dispatch_denial] {
        let (shown, debug) = (err.to_string(), format!("{err:?}"));
        for h in &handles {
            assert!(!shown.contains(h.as_str()) && !debug.contains(h.as_str()), "{shown}");
        }
    }
    for g in fx.db.grants(&fx.id).unwrap() {
        assert!(!format!("{g:?}").contains(&g.handle.to_string()));
    }
}

// ---------- authorization at intent ----------

#[test]
fn record_intent_before_approval_is_denied_not_approved_and_journaled() {
    let fx = unapproved(ALL_CAPS);
    let before = fx.state();
    let n = fx.events().len();
    let err = fx.read().unwrap_err();
    assert_eq!(denied(&err), (Capability::SnapshotRead, "not_approved".to_string()));
    assert_eq!(fx.state(), before, "no effect, no usage, no task change");

    let after = Db::open(&fx.path).unwrap().events(&fx.id).unwrap();
    assert_eq!(after.len(), n + 2, "committed: the decision and the engine's Denied row");
    let decision = &after[n];
    assert_eq!(decision.event_type, "CapabilityDenied");
    assert_eq!(decision.payload["operation"], "snapshot.read");
    assert_eq!(decision.payload["resource"], "task");
    assert_eq!(decision.payload["reason"], "not_approved");
    assert!(decision.payload.get("handle_prefix").is_none(), "no handle was involved");
    let audit = &after[n + 1];
    assert_eq!(audit.event_type, "Denied");
    assert_eq!(audit.payload["reason"], "CapabilityDenied");
    assert_eq!(audit.payload["capability"], "snapshot.read");
}

#[test]
fn authorize_granted_is_journaled_with_operation_and_resource() {
    let fx = approved(ALL_CAPS);
    let n = fx.events().len();
    let e = fx.read().unwrap();
    let after = fx.events();
    assert_eq!(after[n].event_type, "CapabilityGranted", "decided before the effect is intended");
    assert_eq!(after[n + 1].event_type, "EffectIntended");
    let read = &after[n].payload;
    assert_eq!(read["operation"], "snapshot.read");
    assert_eq!(read["resource"], "task");
    assert_eq!(read["handle_prefix"], fx.prefix(Capability::SnapshotRead));
    assert!(read.get("reason").is_none());

    fx.patch(&["src/a.py", "src/b.py"]).unwrap();
    fx.verify().unwrap();
    let granted = fx.of_type("CapabilityGranted");
    assert_eq!(granted.len(), 3);
    assert_eq!(granted[1]["operation"], "workspace.apply_patch");
    assert_eq!(granted[1]["resource"], "paths:2");
    assert_eq!(granted[1]["handle_prefix"], fx.prefix(Capability::WorkspaceApplyPatch));
    assert_eq!(granted[2]["operation"], "verification.run");
    assert_eq!(granted[2]["resource"], "profile:parser-checks-v1");

    // Dispatch re-authorizes and journals that decision too.
    fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap();
    let granted = fx.of_type("CapabilityGranted");
    assert_eq!(granted.len(), 4);
    assert_eq!(granted[3]["operation"], "snapshot.read");
    assert_eq!(granted[3]["handle_prefix"], fx.prefix(Capability::SnapshotRead));
}

#[test]
fn each_denial_reason_is_journaled_exactly_once_and_creates_no_effect() {
    let check = |fx: &Fx, attempt: &dyn Fn(&Fx) -> Result<EffectRecord, DbError>, op: Capability, reason: &str| {
        let before = fx.state();
        let n = fx.of_type("CapabilityDenied").len();
        let err = attempt(fx).unwrap_err();
        assert_eq!(denied(&err), (op, reason.to_string()));
        assert_eq!(fx.state(), before, "{reason}: no effect, no usage, no task change");
        let decisions = fx.of_type("CapabilityDenied");
        assert_eq!(decisions.len(), n + 1, "{reason}: journaled exactly once");
        let d = decisions.last().unwrap();
        assert_eq!(d["operation"], op_name(op));
        assert_eq!(d["reason"], reason);
        d.clone()
    };

    let fx = approved(ALL_CAPS);
    let d = check(&fx, &|fx| fx.patch(&["src/ok.py", "tests/x.py"]), Capability::WorkspaceApplyPatch, "out_of_scope");
    assert_eq!(d["resource"], "paths:2");
    assert_eq!(d["handle_prefix"], fx.prefix(Capability::WorkspaceApplyPatch));

    fx.db.revoke(&fx.id, Some(Capability::SnapshotRead)).unwrap();
    let d = check(&fx, &|fx| fx.read(), Capability::SnapshotRead, "revoked");
    assert_eq!(d["handle_prefix"], fx.prefix(Capability::SnapshotRead));

    fx.set_clock(T0 + DEADLINE);
    let d = check(&fx, &|fx| fx.verify(), Capability::VerificationRun, "expired");
    assert_eq!(d["resource"], "profile:parser-checks-v1");

    let no_verify = approved(&["snapshot.read", "workspace.apply_patch"]);
    let d = check(&no_verify, &|fx| fx.verify(), Capability::VerificationRun, "unknown_handle");
    assert!(d.get("handle_prefix").is_none());
    assert_eq!(no_verify.count("SELECT count(*) FROM effects"), 0);
}

#[test]
fn idempotent_reintent_does_not_journal_a_second_grant() {
    let fx = approved(ALL_CAPS);
    let a = fx.read().unwrap();
    let events = fx.events();
    let b = fx.read().unwrap();
    assert_eq!(a, b);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.of_type("CapabilityGranted").len(), 1);
    // Even after a revoke an existing effect is returned as is: the decision was taken once.
    fx.db.revoke(&fx.id, None).unwrap();
    assert_eq!(fx.read().unwrap(), a);
    assert_eq!(fx.of_type("CapabilityDenied").len(), 0);
}

#[test]
fn check_does_not_journal() {
    let fx = unapproved(ALL_CAPS);
    let events = fx.events();
    let err = fx.db.check(&fx.id, Capability::SnapshotRead, &Resource::Task).unwrap_err();
    assert_eq!(denied(&err), (Capability::SnapshotRead, "not_approved".to_string()));
    fx.db.approve_task(&fx.id).unwrap();
    let events_after_approval = fx.events();
    assert_eq!(events_after_approval.len(), events.len() + 1);

    fx.db.check(&fx.id, Capability::SnapshotRead, &Resource::Task).unwrap();
    fx.db.check(&fx.id, Capability::WorkspaceApplyPatch, &Resource::Paths(vec!["src/a.py".into()])).unwrap();
    fx.db.check(&fx.id, Capability::VerificationRun, &Resource::Profile("parser-checks-v1".into())).unwrap();
    let err = fx.db.check(&fx.id, Capability::WorkspaceApplyPatch, &Resource::Paths(vec!["tests/a.py".into()])).unwrap_err();
    assert_eq!(denied(&err).1, "out_of_scope");
    let err = fx.db.check(&fx.id, Capability::VerificationRun, &Resource::Profile("other".into())).unwrap_err();
    assert_eq!(denied(&err).1, "out_of_scope");
    fx.set_clock(T0 + DEADLINE);
    assert_eq!(denied(&fx.db.check(&fx.id, Capability::SnapshotRead, &Resource::Task).unwrap_err()).1, "expired");
    fx.db.revoke(&fx.id, Some(Capability::ArtifactExport)).unwrap();
    let revoked_events = fx.events();
    assert_eq!(denied(&fx.db.check(&fx.id, Capability::ArtifactExport, &Resource::Task).unwrap_err()).1, "revoked");
    assert_eq!(fx.events(), revoked_events);
    assert_eq!(fx.events().len(), events_after_approval.len() + 1, "only the revoke was journaled");

    let partial = approved(&["snapshot.read"]);
    let err = partial.db.check(&partial.id, Capability::VerificationRun, &Resource::Profile("parser-checks-v1".into()));
    assert_eq!(denied(&err.unwrap_err()).1, "unknown_handle");
    assert!(matches!(fx.db.check(&TaskId::new(), Capability::SnapshotRead, &Resource::Task), Err(DbError::NotFound(_))));
}

// ---------- dispatch and revocation ----------

#[test]
fn mark_dispatched_denies_after_a_revoke_and_journals_it() {
    let fx = approved(ALL_CAPS);
    let e = fx.read().unwrap();
    fx.db.revoke(&fx.id, Some(Capability::SnapshotRead)).unwrap();
    let (task, usage) = (fx.task(), fx.usage());
    let n = fx.events().len();
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap_err();
    assert_eq!(denied(&err), (Capability::SnapshotRead, "revoked".to_string()));
    assert_eq!(fx.db.effect(&e.effect_id).unwrap(), e, "still INTENDED, lease untouched");
    assert_eq!(fx.count("SELECT count(*) FROM attempts"), 0);
    assert_eq!((fx.task(), fx.usage()), (task, usage));

    let after = Db::open(&fx.path).unwrap().events(&fx.id).unwrap();
    assert_eq!(after.len(), n + 1, "the denial is committed; nothing else");
    let d = &after[n];
    assert_eq!(d.event_type, "CapabilityDenied");
    assert_eq!(d.payload["operation"], "snapshot.read");
    assert_eq!(d.payload["reason"], "revoked");
    assert_eq!(d.payload["handle_prefix"], fx.prefix(Capability::SnapshotRead));
    assert_eq!(fx.of_type("EffectDispatched").len(), 0);

    // Expiry is re-checked at dispatch as well.
    let fx = approved(ALL_CAPS);
    let e = fx.read().unwrap();
    fx.set_clock(T0 + DEADLINE);
    let err = fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap_err();
    assert_eq!(denied(&err), (Capability::SnapshotRead, "expired".to_string()));
    assert_eq!(fx.db.effect(&e.effect_id).unwrap().state, EffectState::Intended);
}

#[test]
fn revoke_blocks_new_intents_but_keeps_completed_effects() {
    let fx = approved(ALL_CAPS);
    let e = fx.read().unwrap();
    let a = AttemptId::new();
    fx.db.mark_dispatched(&e.effect_id, &a, "w", 1).unwrap();
    let art = Digest::of(b"manifest");
    fx.db.register_artifact(&art, 8, "result", Some(&e.effect_id), "w").unwrap();
    let r = Receipt {
        effect_id: e.effect_id.clone(),
        attempt_id: a,
        lease_generation: 1,
        outcome: Outcome::Success,
        result_digest: Some(art),
    };
    fx.db.complete_effect(&e.effect_id, &r, Some(&art), None).unwrap();
    let done = fx.db.effect(&e.effect_id).unwrap();
    let usage = fx.usage();

    let revoked = fx.db.revoke(&fx.id, None).unwrap();
    assert_eq!(revoked.len(), 4);
    assert!(fx.db.grants(&fx.id).unwrap().iter().all(|g| g.revoked));
    let events = fx.of_type("CapabilityRevoked");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["operations"].as_array().unwrap().len(), 4);

    assert_eq!(denied(&fx.verify().unwrap_err()).1, "revoked");
    assert_eq!(denied(&fx.patch(&["src/a.py"]).unwrap_err()).1, "revoked");
    assert_eq!(fx.db.effect(&e.effect_id).unwrap(), done);
    assert_eq!(done.state, EffectState::Completed);
    assert_eq!(fx.usage(), usage);
    assert_eq!(fx.count("SELECT count(*) FROM effects"), 1);
}

#[test]
fn revoke_one_capability_leaves_others() {
    let fx = approved(ALL_CAPS);
    let n = fx.events().len();
    assert_eq!(fx.db.revoke(&fx.id, Some(Capability::VerificationRun)).unwrap(), vec![Capability::VerificationRun]);
    let after = fx.events();
    assert_eq!(after.len(), n + 1);
    assert_eq!(after[n].event_type, "CapabilityRevoked");
    assert_eq!(after[n].payload["operations"], json!(["verification.run"]));
    assert_eq!(after[n].payload["handles"][0]["prefix"], fx.prefix(Capability::VerificationRun));

    assert_eq!(denied(&fx.verify().unwrap_err()).1, "revoked");
    fx.read().unwrap();
    fx.patch(&["src/a.py"]).unwrap();
    let revoked: Vec<_> = fx.db.grants(&fx.id).unwrap().into_iter().filter(|g| g.revoked).map(|g| g.operation).collect();
    assert_eq!(revoked, vec![Capability::VerificationRun]);
}

#[test]
fn revoke_twice_second_is_a_noop_without_event() {
    let fx = approved(ALL_CAPS);
    assert_eq!(fx.db.revoke(&fx.id, Some(Capability::SnapshotRead)).unwrap(), vec![Capability::SnapshotRead]);
    let events = fx.events();
    assert!(fx.db.revoke(&fx.id, Some(Capability::SnapshotRead)).unwrap().is_empty());
    assert_eq!(fx.events(), events);
    // Revoking everything only reports what actually changed.
    assert_eq!(fx.db.revoke(&fx.id, None).unwrap().len(), 3);
    let events = fx.events();
    assert!(fx.db.revoke(&fx.id, None).unwrap().is_empty());
    assert_eq!(fx.events(), events);
    // An operation the task was never granted, or an unapproved task: nothing to revoke.
    let partial = approved(&["snapshot.read"]);
    assert!(partial.db.revoke(&partial.id, Some(Capability::ArtifactExport)).unwrap().is_empty());
    let fresh = unapproved(ALL_CAPS);
    let events = fresh.events();
    assert!(fresh.db.revoke(&fresh.id, None).unwrap().is_empty());
    assert_eq!(fresh.events(), events);
    assert!(matches!(fx.db.revoke(&TaskId::new(), None), Err(DbError::NotFound(_))));
}

#[test]
fn artifact_export_handle_has_no_expiry() {
    let fx = approved(ALL_CAPS);
    fx.set_clock(i64::MAX / 2);
    assert!(fx.db.deadline_passed(&fx.id).unwrap());
    fx.db.check(&fx.id, Capability::ArtifactExport, &Resource::Task).unwrap();
    assert_eq!(denied(&fx.db.check(&fx.id, Capability::SnapshotRead, &Resource::Task).unwrap_err()).1, "expired");
    let e = fx.export().unwrap();
    assert_eq!(e.state, EffectState::Intended);
    fx.db.mark_dispatched(&e.effect_id, &AttemptId::new(), "w", 1).unwrap();
    // Still revocable.
    fx.db.revoke(&fx.id, Some(Capability::ArtifactExport)).unwrap();
    assert_eq!(denied(&fx.db.check(&fx.id, Capability::ArtifactExport, &Resource::Task).unwrap_err()).1, "revoked");
}

// ---------- schema ----------

fn open_err(path: &Path) -> DbError {
    Db::open(path).err().expect("open is refused")
}

#[test]
fn reopening_a_user_version_0_database_with_tables_is_refused() {
    for old in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentos.db");
        drop(Db::open(&path).unwrap());
        Connection::open(&path).unwrap().pragma_update(None, "user_version", old).unwrap();
        let err = open_err(&path);
        let msg = err.to_string();
        assert!(msg.contains("database was created by an older Agent OS; v0.1 databases are not migrated"), "{old}: {msg}");
        // Refusing changes nothing: the file keeps its old version.
        let v: i64 = Connection::open(&path).unwrap().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, old);
    }
}

#[test]
fn reopening_a_version_3_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    drop(Db::open(&path).unwrap());
    Connection::open(&path).unwrap().pragma_update(None, "user_version", 3).unwrap();
    let err = open_err(&path);
    assert!(matches!(err, DbError::SchemaVersion { found: 3, supported: 2 }), "{err:?}");
}

#[test]
fn a_fresh_database_gets_version_2() {
    assert_eq!(SCHEMA_VERSION, 2);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    // An empty file at version 0 without tables is a fresh database, not an old one.
    drop(Connection::open(&path).unwrap());
    let db = Db::open(&path).unwrap();
    assert_eq!(db.pragma_string("user_version").unwrap(), "2");
    drop(db);
    drop(Db::open(&path).unwrap());

    let conn = Connection::open(&path).unwrap();
    let mut stmt = conn.prepare("SELECT name, \"notnull\" FROM pragma_table_info('capabilities')").unwrap();
    let cols: Vec<(String, bool)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    for (name, notnull) in [("scope", true), ("created_ts", true), ("expires_ts", false), ("revoked", true)] {
        assert!(cols.contains(&(name.to_string(), notnull)), "capabilities.{name} notnull={notnull}: {cols:?}");
    }
}

#[test]
fn reserved_event_names_cannot_be_forged_via_append_audit() {
    let fx = approved(ALL_CAPS);
    let events = fx.events();
    for name in ["CapabilitiesIssued", "CapabilityGranted", "CapabilityDenied", "CapabilityRevoked"] {
        let err = fx.db.append_audit(&fx.id, name, &json!({})).unwrap_err();
        assert!(matches!(err, DbError::ReservedEventType(ref t) if t == name), "{name}: {err:?}");
    }
    assert_eq!(fx.events(), events);
}

// ---------- fault injection ----------

#[test]
fn trigger_aborting_the_capability_granted_insert_makes_record_intent_write_no_effect_no_usage_no_task_change() {
    let fx = approved(ALL_CAPS);
    Connection::open(&fx.path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_grant BEFORE INSERT ON events WHEN NEW.type='CapabilityGranted'
             BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .unwrap();
    let before = fx.state();
    let events = fx.events();
    assert!(matches!(fx.read(), Err(DbError::Sqlite(_))));
    assert_eq!(fx.state(), before);
    assert_eq!(fx.events(), events);
    assert_eq!(fx.count("SELECT count(*) FROM attempts"), 0);
    // The connection stays usable.
    fx.db.append_audit(&fx.id, "OperatorNote", &json!({})).unwrap();
}
