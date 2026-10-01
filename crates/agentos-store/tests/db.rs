use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;

use agentos_core::contract::Contract;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{TaskEvent, TaskState, TransitionError};
use agentos_store::db::{Db, DbError};
use rusqlite::Connection;

fn contract() -> (Contract, Digest) {
    let json = r#"{
        "goal": "fix the parser",
        "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
        "profile": "protected",
        "editable_paths": ["src/"],
        "verification_profile": "parser-checks-v1",
        "capabilities": ["snapshot.read", "workspace.apply_patch"],
        "limits": {
            "model_requests": 10,
            "max_output_tokens_per_request": 1000,
            "tool_actions": 1000,
            "deadline_seconds": 600,
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }
    }"#;
    let c = Contract::parse(json).unwrap();
    let d = Digest::of(json.as_bytes());
    (c, d)
}

fn open(dir: &tempfile::TempDir) -> Db {
    Db::open(&dir.path().join("agentos.db")).unwrap()
}

fn running(db: &Db) -> TaskId {
    let (c, d) = contract();
    let id = db.create_task(&c, &d).unwrap();
    db.append(&id, &TaskEvent::Started).unwrap();
    id
}

#[test]
fn create_task_writes_ready_task_and_first_event() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let (c, d) = contract();
    let id = db.create_task(&c, &d).unwrap();
    let t = db.task(&id).unwrap();
    assert_eq!(t.state, TaskState::Ready);
    assert_eq!(t.workspace_digest, Digest::of(b"abc123"));
    assert_eq!((t.step, t.actions_used, t.cancel_requested), (0, 0, false));
    let evs = db.events(&id).unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!((evs[0].seq, evs[0].event_type.as_str()), (1, "TaskCreated"));
}

#[test]
fn two_tasks_from_same_contract_get_distinct_ids() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let (c, d) = contract();
    let a = db.create_task(&c, &d).unwrap();
    let b = db.create_task(&c, &d).unwrap();
    assert_ne!(a, b);
}

#[test]
fn invalid_transition_leaves_tasks_and_events_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let id = running(&db);
    let task_before = db.task(&id).unwrap();
    let events_before = db.events(&id).unwrap();

    let err = db.append(&id, &TaskEvent::Started).unwrap_err();
    assert!(matches!(
        err,
        DbError::Transition(TransitionError::InvalidTransition { .. })
    ));
    assert_eq!(db.task(&id).unwrap(), task_before);
    assert_eq!(db.events(&id).unwrap(), events_before);
}

#[test]
fn sequences_are_gapless_and_payload_preserves_event() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let id = running(&db);
    for _ in 0..5 {
        db.append(&id, &TaskEvent::ActionUsed).unwrap();
    }
    db.append(&id, &TaskEvent::Failed { reason: "boom".into() }).unwrap();
    let evs = db.events(&id).unwrap();
    let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<_>>());
    assert_eq!(evs[0].event_type, "TaskCreated");
    assert_eq!(evs[1].event_type, "Started");
    let last = evs.last().unwrap();
    assert_eq!(last.event_type, "Failed");
    assert_eq!(
        serde_json::from_value::<TaskEvent>(last.payload.clone()).unwrap(),
        TaskEvent::Failed { reason: "boom".into() }
    );
    assert_eq!(db.task(&id).unwrap().state, TaskState::Failed);
    assert_eq!(db.task(&id).unwrap().actions_used, 5);
}

#[test]
fn eight_threads_with_own_handles_get_unique_gapless_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let id = running(&Db::open(&path).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let per_thread = 10;
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (path, id, barrier) = (path.clone(), id.clone(), barrier.clone());
            thread::spawn(move || {
                let db = Db::open(&path).unwrap();
                barrier.wait();
                for j in 0..per_thread {
                    if (i + j) % 2 == 0 {
                        db.append(&id, &TaskEvent::ActionUsed).unwrap();
                    } else {
                        db.append_audit(&id, "Audit", &serde_json::json!({"t": i})).unwrap();
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let db = Db::open(&path).unwrap();
    let evs = db.events(&id).unwrap();
    assert_eq!(evs.len(), 2 + 8 * per_thread);
    let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
    assert_eq!(seqs.iter().copied().collect::<HashSet<_>>().len(), seqs.len());
    assert_eq!(seqs, (1..=evs.len() as u64).collect::<Vec<_>>());
    let actions = evs.iter().filter(|e| e.event_type == "ActionUsed").count();
    assert_eq!(db.task(&id).unwrap().actions_used as usize, actions);
}

#[test]
fn reopening_the_file_yields_identical_task_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let (task, events, id) = {
        let db = Db::open(&path).unwrap();
        let id = running(&db);
        db.append(&id, &TaskEvent::ActionUsed).unwrap();
        db.append(&id, &TaskEvent::WorkspaceUpdated { digest: Digest::of(b"w2") }).unwrap();
        db.append(&id, &TaskEvent::VerifyStarted).unwrap();
        db.append(&id, &TaskEvent::VerifyPassed { digest: Digest::of(b"w2") }).unwrap();
        (db.task(&id).unwrap(), db.events(&id).unwrap(), id)
    };
    let db = Db::open(&path).unwrap();
    assert_eq!(db.task(&id).unwrap(), task);
    assert_eq!(db.events(&id).unwrap(), events);
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.verified_digest, Some(Digest::of(b"w2")));
}

#[test]
fn pragmas_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert_eq!(db.pragma_string("journal_mode").unwrap(), "wal");
    assert_eq!(db.pragma_string("synchronous").unwrap(), "2");
    assert_eq!(db.pragma_string("foreign_keys").unwrap(), "1");
    assert_eq!(db.pragma_string("busy_timeout").unwrap(), "5000");
    drop(db);
    let db = open(&dir);
    assert_eq!(db.pragma_string("journal_mode").unwrap(), "wal");
}

#[test]
fn missing_task_is_not_found_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ghost = TaskId::new();
    assert!(matches!(db.append(&ghost, &TaskEvent::Started), Err(DbError::NotFound(_))));
    assert!(matches!(db.task(&ghost), Err(DbError::NotFound(_))));
    assert!(matches!(db.events(&ghost), Err(DbError::NotFound(_))));
    assert!(matches!(
        db.append_audit(&ghost, "Denied", &serde_json::json!({})),
        Err(DbError::NotFound(_))
    ));
}

#[test]
fn foreign_key_is_enforced_at_the_schema_level() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    drop(Db::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let r = conn.execute(
        "INSERT INTO events(task_id, seq, type, payload, ts) VALUES ('nope', 1, 'X', '{}', 0)",
        [],
    );
    assert!(r.is_err());
}

#[test]
fn audit_events_keep_seq_gapless_and_leave_task_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let id = running(&db);
    let before = db.task(&id).unwrap();
    let s1 = db.append_audit(&id, "CapabilityDenied", &serde_json::json!({"why": "x"})).unwrap();
    assert_eq!(s1, 3);
    assert_eq!(db.task(&id).unwrap(), before);
    db.append(&id, &TaskEvent::ActionUsed).unwrap();
    let s2 = db.append_audit(&id, "ReceiptIgnored", &serde_json::json!({})).unwrap();
    assert_eq!(s2, 5);
    let seqs: Vec<u64> = db.events(&id).unwrap().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    let evs = db.events(&id).unwrap();
    assert_eq!(evs[2].event_type, "CapabilityDenied");
    assert_eq!(evs[2].payload, serde_json::json!({"why": "x"}));
}

#[test]
fn uncommitted_transaction_leaves_no_partial_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let db = Db::open(&path).unwrap();
    let id = running(&db);
    let task_before = db.task(&id).unwrap();
    let events_before = db.events(&id).unwrap();

    // Simulate a crash mid-append: write state and event, then drop without commit.
    {
        let mut conn = Connection::open(&path).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute("UPDATE tasks SET actions_used = 99, step = 99 WHERE id = ?1", [id.as_str()])
            .unwrap();
        tx.execute(
            "INSERT INTO events(task_id, seq, type, payload, ts) VALUES (?1, 3, 'ActionUsed', '{}', 0)",
            [id.as_str()],
        )
        .unwrap();
        drop(tx);
    }

    let reopened = Db::open(&path).unwrap();
    assert_eq!(reopened.task(&id).unwrap(), task_before);
    assert_eq!(reopened.events(&id).unwrap(), events_before);
}

#[test]
fn all_spec_tables_exist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    drop(Db::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    for t in [
        "tasks", "events", "effects", "attempts", "artifacts", "usage", "capabilities",
        "observations",
    ] {
        let n: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1", [t], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "missing table {t}");
    }
}


#[test]
fn reads_do_not_block_on_an_open_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let writer = Db::open(&path).unwrap();
    let id = running(&writer);
    let before_task = writer.task(&id).unwrap();
    let before_events = writer.events(&id).unwrap();

    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    raw.execute("UPDATE tasks SET actions_used = 42 WHERE id = ?1", [id.as_str()]).unwrap();

    let reader = Db::open(&path).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(reader.task(&id).unwrap(), before_task);
    assert_eq!(reader.events(&id).unwrap(), before_events);
    assert!(start.elapsed() < std::time::Duration::from_millis(500), "{:?}", start.elapsed());
    raw.execute_batch("ROLLBACK").unwrap();
}

fn inject_failure(path: &std::path::Path, event_type: &str) {
    let raw = Connection::open(path).unwrap();
    raw.execute_batch(&format!(
        "CREATE TRIGGER fail_ins BEFORE INSERT ON events WHEN NEW.type='{event_type}'
         BEGIN SELECT RAISE(ABORT,'injected'); END;"
    ))
    .unwrap();
}

#[test]
fn failed_event_insert_rolls_back_the_state_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let db = Db::open(&path).unwrap();
    let id = running(&db);
    let task_before = db.task(&id).unwrap();
    let events_before = db.events(&id).unwrap();
    inject_failure(&path, "ActionUsed");

    assert!(matches!(db.append(&id, &TaskEvent::ActionUsed), Err(DbError::Sqlite(_))));
    assert_eq!(db.task(&id).unwrap(), task_before);
    assert_eq!(db.events(&id).unwrap(), events_before);
    // Handle remains usable afterwards.
    db.append(&id, &TaskEvent::Paused).unwrap();
}

#[test]
fn failed_create_task_leaves_no_task_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentos.db");
    let db = Db::open(&path).unwrap();
    inject_failure(&path, "TaskCreated");
    let (c, d) = contract();
    assert!(db.create_task(&c, &d).is_err());
    let raw = Connection::open(&path).unwrap();
    let n: i64 = raw.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}
