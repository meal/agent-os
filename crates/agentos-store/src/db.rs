use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_core::contract::{Contract, Limits};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{reduce, Task, TaskEvent, TaskState, TransitionError};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

const OWNER: &str = "local-owner";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks(
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    contract_digest TEXT NOT NULL,
    contract_json TEXT NOT NULL,
    state TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL,
    workspace_digest TEXT NOT NULL,
    verified_digest TEXT,
    actions_used INTEGER NOT NULL,
    step INTEGER NOT NULL,
    checkpoint TEXT,
    deadline_ts INTEGER NOT NULL,
    created_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events(
    task_id TEXT NOT NULL REFERENCES tasks(id),
    seq INTEGER NOT NULL,
    type TEXT NOT NULL,
    payload TEXT NOT NULL,
    ts INTEGER NOT NULL,
    refs TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY(task_id, seq)
);
CREATE TABLE IF NOT EXISTS effects(
    effect_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    step INTEGER NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    expected_workspace TEXT,
    lease_generation INTEGER NOT NULL DEFAULT 0,
    result_digest TEXT,
    created_ts INTEGER NOT NULL,
    updated_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS attempts(
    attempt_id TEXT PRIMARY KEY,
    effect_id TEXT NOT NULL REFERENCES effects(effect_id),
    worker TEXT NOT NULL,
    lease_generation INTEGER NOT NULL,
    started_ts INTEGER NOT NULL,
    finished_ts INTEGER
);
CREATE TABLE IF NOT EXISTS artifacts(
    digest TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    type TEXT NOT NULL,
    effect_id TEXT,
    provenance TEXT,
    created_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    effect_id TEXT,
    kind TEXT NOT NULL,
    reserved INTEGER NOT NULL,
    settled INTEGER,
    status TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS capabilities(
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    resource TEXT NOT NULL,
    operation TEXT NOT NULL,
    expires_ts INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS observations(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    source TEXT NOT NULL,
    observed_revision TEXT NOT NULL,
    observed_ts INTEGER NOT NULL,
    evidence_ref TEXT
);
";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("transition rejected: {0}")]
    Transition(#[from] TransitionError),
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub task_id: TaskId,
    pub seq: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub ts: i64,
}

/// One SQLite connection. Not shared across threads: each thread opens its own `Db`.
pub struct Db {
    conn: Connection,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn state_to_str(s: TaskState) -> Result<String> {
    match serde_json::to_value(s)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(DbError::Corrupt(format!("state serialized as {other}"))),
    }
}

fn state_from_str(s: String) -> Result<TaskState> {
    Ok(serde_json::from_value(serde_json::Value::String(s))?)
}

fn digest_from_str(s: &str) -> Result<Digest> {
    Digest::from_hex(s).map_err(|e| DbError::Corrupt(e.to_string()))
}

fn event_name(ev: &TaskEvent) -> Result<String> {
    // Externally tagged: unit variants serialize as a string, others as {"Name": ...}.
    match serde_json::to_value(ev)? {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Object(m) if m.len() == 1 => Ok(m.keys().next().cloned().unwrap_or_default()),
        other => Err(DbError::Corrupt(format!("event serialized as {other}"))),
    }
}

fn load_task(tx: &Transaction, id: &TaskId) -> Result<(Task, Limits)> {
    let row = tx
        .query_row(
            "SELECT state, cancel_requested, workspace_digest, verified_digest, actions_used, step, contract_json
             FROM tasks WHERE id = ?1",
            [id.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| DbError::NotFound(id.clone()))?;
    let contract: Contract = serde_json::from_str(&row.6)?;
    let task = Task {
        id: id.clone(),
        state: state_from_str(row.0)?,
        cancel_requested: row.1,
        workspace_digest: digest_from_str(&row.2)?,
        verified_digest: row.3.as_deref().map(digest_from_str).transpose()?,
        actions_used: row.4,
        step: row.5,
    };
    Ok((task, contract.limits))
}

fn insert_event(
    tx: &Transaction,
    id: &TaskId,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<u64> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE task_id = ?1",
        [id.as_str()],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO events(task_id, seq, type, payload, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.as_str(), seq, event_type, serde_json::to_string(payload)?, now_ts()],
    )?;
    Ok(seq as u64)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        // busy_timeout first so the remaining setup also waits out concurrent openers.
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(DbError::Corrupt(format!("journal_mode is {mode}, expected wal")));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    /// Current value of a PRAGMA, rendered as text (diagnostics and tests).
    pub fn pragma_string(&self, name: &str) -> Result<String> {
        let v: rusqlite::types::Value = self
            .conn
            .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?;
        Ok(match v {
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Text(s) => s,
            other => format!("{other:?}"),
        })
    }

    fn immediate(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?)
    }

    /// Snapshot read: deferred, so it never takes the write lock and does not block writers under WAL.
    fn read(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?)
    }

    pub fn create_task(&self, contract: &Contract, contract_digest: &Digest) -> Result<TaskId> {
        let id = TaskId::new();
        // Placeholder base workspace digest until real workspace digests exist (Task 9).
        let base = Digest::of(contract.repository.revision.as_bytes());
        let task = Task::new(id.clone(), base);
        let now = now_ts();
        let deadline = now + i64::from(contract.limits.deadline_seconds);
        let tx = self.immediate()?;
        tx.execute(
            "INSERT INTO tasks(id, owner, contract_digest, contract_json, state, cancel_requested,
                workspace_digest, verified_digest, actions_used, step, checkpoint, deadline_ts, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, NULL, 0, 0, NULL, ?7, ?8)",
            params![
                id.as_str(),
                OWNER,
                contract_digest.to_string(),
                serde_json::to_string(contract)?,
                state_to_str(task.state)?,
                task.workspace_digest.to_string(),
                deadline,
                now
            ],
        )?;
        insert_event(&tx, &id, "TaskCreated", &serde_json::json!({ "contract_digest": contract_digest.to_string() }))?;
        tx.commit()?;
        Ok(id)
    }

    pub fn append(&self, id: &TaskId, ev: &TaskEvent) -> Result<Task> {
        let tx = self.immediate()?;
        let (task, limits) = load_task(&tx, id)?;
        // On error `tx` is dropped, which rolls back; nothing has been written yet anyway.
        let next = reduce(&task, ev, &limits)?;
        tx.execute(
            "UPDATE tasks SET state = ?2, cancel_requested = ?3, workspace_digest = ?4,
                verified_digest = ?5, actions_used = ?6, step = ?7 WHERE id = ?1",
            params![
                id.as_str(),
                state_to_str(next.state)?,
                next.cancel_requested,
                next.workspace_digest.to_string(),
                next.verified_digest.map(|d| d.to_string()),
                next.actions_used,
                next.step
            ],
        )?;
        insert_event(&tx, id, &event_name(ev)?, &serde_json::to_value(ev)?)?;
        tx.commit()?;
        Ok(next)
    }

    /// Append an event row without touching task state (denials, ignored receipts).
    pub fn append_audit(&self, id: &TaskId, event_type: &str, payload: &serde_json::Value) -> Result<u64> {
        let tx = self.immediate()?;
        load_task(&tx, id)?;
        let seq = insert_event(&tx, id, event_type, payload)?;
        tx.commit()?;
        Ok(seq)
    }

    pub fn task(&self, id: &TaskId) -> Result<Task> {
        let tx = self.read()?;
        Ok(load_task(&tx, id)?.0)
    }

    pub fn events(&self, id: &TaskId) -> Result<Vec<StoredEvent>> {
        let tx = self.read()?;
        load_task(&tx, id)?;
        let mut stmt = tx.prepare(
            "SELECT seq, type, payload, ts FROM events WHERE task_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([id.as_str()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, event_type, payload, ts) = row?;
            out.push(StoredEvent {
                task_id: id.clone(),
                seq: seq as u64,
                event_type,
                payload: serde_json::from_str(&payload)?,
                ts,
            });
        }
        Ok(out)
    }
}
