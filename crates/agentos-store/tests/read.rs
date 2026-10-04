//! Read views must page deterministically, reject before decoding large payloads,
//! and leave durable state untouched.
use agentos_core::broker::Resource;
use agentos_core::budget::Reservation;
use agentos_core::contract::Contract;
use agentos_core::effect::EffectKind;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_store::db::{Db, DbError};
use rusqlite::{Connection, params};

fn fixture() -> (tempfile::TempDir, Db, Contract) {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(&root.path().join("agentos.db")).unwrap();
    let c = Contract::parse(
        r#"{
      "goal":"fix the parser", "repository":{"source":"fixtures/parser-repo","revision":"abc"},
      "profile":"protected", "editable_paths":["src/"], "verification_profile":"parser-checks-v1",
      "capabilities":["snapshot.read","workspace.apply_patch","verification.run"],
      "limits":{"model_requests":10,"max_output_tokens_per_request":1000,"tool_actions":1000,
                "deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}
    }"#,
    )
    .unwrap();
    (root, db, c)
}
fn create(db: &Db, c: &Contract) -> TaskId {
    db.create_task(c, &Digest::of(serde_json::to_string(c).unwrap().as_bytes()))
        .unwrap()
}

#[test]
fn timestamp_ties_page_without_duplicates_and_reject_other_filter() {
    let (root, db, c) = fixture();
    let mut want: Vec<_> = (0..3).map(|_| create(&db, &c)).collect();
    Connection::open(root.path().join("agentos.db"))
        .unwrap()
        .execute("UPDATE tasks SET created_ts=100", [])
        .unwrap();
    want.sort_by(|a, b| b.as_str().cmp(a.as_str()));
    let first = db.tasks_page(None, None, 2).unwrap();
    let second = db.tasks_page(None, first.next.as_ref(), 2).unwrap();
    let ids: Vec<_> = first
        .rows
        .iter()
        .chain(&second.rows)
        .map(|r| r.task.id.clone())
        .collect();
    assert_eq!(ids, want);
    assert!(second.next.is_none());
    assert!(matches!(
        db.tasks_page(Some(TaskState::Ready), first.next.as_ref(), 2),
        Err(DbError::InvalidQuery(_))
    ));
    db.approve_task(&want[0]).unwrap();
    db.append(&want[0], &TaskEvent::Started).unwrap();
    let page = db.tasks_page(Some(TaskState::Ready), None, 50).unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|r| r.task.state == TaskState::Ready));
}

#[test]
fn list_labels_are_byte_bounded_without_splitting_unicode() {
    let (_, db, mut c) = fixture();
    c.goal = "é".repeat(3000);
    create(&db, &c);
    let row = db.tasks_page(None, None, 50).unwrap().rows.remove(0);
    assert_eq!(row.goal, "é".repeat(2048));
    assert!(row.text_truncated);
}

#[test]
fn oversized_legacy_contract_is_listable_but_not_decoded_for_detail() {
    let (root, db, c) = fixture();
    let id = create(&db, &c);
    let conn = Connection::open(root.path().join("agentos.db")).unwrap();
    // Deliberately invalid as well as oversized: a JSON parse would fail first.
    conn.execute(
        "UPDATE tasks SET contract_json=?1 WHERE id=?2",
        params!["x".repeat(300_000), id.as_str()],
    )
    .unwrap();
    let page = db.tasks_page(None, None, 10).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert!(page.rows[0].text_truncated);
    assert!(matches!(
        db.contract_bounded(&id, 262_144),
        Err(DbError::ReadLimit { limit: 262_144 })
    ));
    assert!(!db.event_headers(&id, 0, 10).unwrap().rows.is_empty());
}

#[test]
fn events_page_by_sequence_and_do_not_decode_large_payloads() {
    let (root, db, c) = fixture();
    let id = create(&db, &c);
    for n in 0..3 {
        db.append_audit(&id, "Submitted", &serde_json::json!({"n":n}))
            .unwrap();
    }
    let first = db.event_headers(&id, 0, 2).unwrap();
    assert_eq!(first.rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
    assert!(first.has_more);
    let second = db.event_headers(&id, first.last_seq, 2).unwrap();
    assert_eq!(
        second.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(!second.has_more);
    let conn = Connection::open(root.path().join("agentos.db")).unwrap();
    conn.execute(
        "UPDATE events SET payload=?1 WHERE task_id=?2 AND seq=2",
        params!["x".repeat(70_000), id.as_str()],
    )
    .unwrap();
    assert!(db.event_headers(&id, 1, 1).unwrap().rows[0].truncated);
    assert!(matches!(
        db.events_bounded(&id, 65_536),
        Err(DbError::ReadLimit { limit: 65_536 })
    ));
    assert!(matches!(
        db.first_event_bounded(&id, "Submitted", Some(65_536)),
        Err(DbError::ReadLimit { .. })
    ));
}

#[test]
fn event_budget_is_aggregate_and_first_submitted_is_preserved() {
    let (_, db, c) = fixture();
    let id = create(&db, &c);
    db.append_audit(&id, "Submitted", &serde_json::json!({"model":"first"}))
        .unwrap();
    db.append_audit(&id, "Submitted", &serde_json::json!({"model":"second"}))
        .unwrap();
    assert_eq!(
        db.first_event_bounded(&id, "Submitted", Some(1000))
            .unwrap()
            .unwrap()
            .payload["model"],
        "first"
    );
    assert!(matches!(
        db.events_bounded(&id, 25),
        Err(DbError::ReadLimit { .. })
    ));
    assert_eq!(
        db.events_bounded(&id, 10_000).unwrap(),
        db.events(&id).unwrap()
    );
    assert!(
        db.first_event_bounded(&id, "absent", Some(0))
            .unwrap()
            .is_none()
    );
}

#[test]
fn read_queries_leave_all_durable_rows_unchanged() {
    let (_, db, c) = fixture();
    let id = create(&db, &c);
    let task = db.task(&id).unwrap();
    let before = db.events(&id).unwrap();
    db.tasks_page(None, None, 50).unwrap();
    db.event_headers(&id, 0, 100).unwrap();
    assert_eq!(db.contract_bounded(&id, 262_144).unwrap(), c);
    db.events_bounded(&id, 10000).unwrap();
    assert_eq!(db.events(&id).unwrap(), before);
    assert_eq!(db.task(&id).unwrap(), task);
    assert!(db.grants(&id).unwrap().is_empty());
    assert_eq!(db.usage_summary(&id).unwrap(), Default::default());
}

#[test]
fn invalid_limits_and_unknown_tasks_are_refused() {
    let (_, db, _) = fixture();
    for limit in [0, 51, usize::MAX] {
        assert!(matches!(
            db.tasks_page(None, None, limit),
            Err(DbError::InvalidQuery(_))
        ));
    }
    let unknown = TaskId::new();
    for limit in [0, 101, usize::MAX] {
        assert!(matches!(
            db.event_headers(&unknown, 0, limit),
            Err(DbError::InvalidQuery(_))
        ));
    }
    assert!(matches!(
        db.event_headers(&unknown, u64::MAX, 10),
        Err(DbError::InvalidQuery(_))
    ));
    assert!(matches!(
        db.event_headers(&unknown, 0, 10),
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        db.events_bounded(&unknown, 100),
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        db.contract_bounded(&unknown, 100),
        Err(DbError::NotFound(_))
    ));
}

#[test]
fn effect_metadata_is_budgeted_before_decoding() {
    let (root, db, c) = fixture();
    let id = create(&db, &c);
    db.approve_task(&id).unwrap();
    db.append(&id, &TaskEvent::Started).unwrap();
    let rec = db
        .record_intent(
            &id,
            EffectKind::ReadSnapshot,
            Digest::of(b"request"),
            &db.task(&id).unwrap().workspace_digest,
            Reservation::for_kind(&EffectKind::ReadSnapshot, 0),
            &Resource::Task,
        )
        .unwrap();
    assert_eq!(db.effect_bounded(&rec.effect_id, 1000).unwrap(), rec);
    assert_eq!(
        db.outstanding_effects_bounded(&id, 1000).unwrap(),
        vec![rec.clone()]
    );
    Connection::open(root.path().join("agentos.db"))
        .unwrap()
        .execute(
            "UPDATE effects SET kind=?1 WHERE effect_id=?2",
            params!["x".repeat(2000), rec.effect_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        db.effect_bounded(&rec.effect_id, 1000),
        Err(DbError::ReadLimit { .. })
    ));
    assert!(matches!(
        db.outstanding_effects_bounded(&id, 1000),
        Err(DbError::ReadLimit { .. })
    ));
}
