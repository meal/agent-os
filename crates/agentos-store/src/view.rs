//! A read-only window onto the store for the cockpit. `View` is a separate type from `Db`
//! so that nothing reachable from it can write.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentos_core::contract::Contract;
use agentos_core::ids::{TaskId, WorkflowId};
use agentos_core::state::TaskState;
use agentos_core::workflow::{ChainSpec, StepStatus, WorkflowState, state_of_steps};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::Value;

use crate::db::{SCHEMA_VERSION, state_from_str};

pub struct View {
    conn: Connection,
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("no agent-os database at {0}")]
    Missing(PathBuf),
    #[error("cannot open {}: {source}", path.display())]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "database schema version {found} is not {expected}; run any other agentos command to upgrade it"
    )]
    Schema { found: i64, expected: i64 },
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
}

type Result<T> = std::result::Result<T, ViewError>;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub tasks: Vec<TaskRow>,
    pub workflows: Vec<WorkflowRow>,
    pub more: bool,
    /// Rows left out because a stored value in them could not be read.
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: TaskId,
    pub state: TaskState,
    pub cancel_requested: bool,
    pub goal: String,
    pub model_limit: u32,
    pub tool_limit: u32,
    pub model_used: u64,
    pub tool_used: u64,
    pub created_ts: i64,
}

#[derive(Debug, Clone)]
pub struct StepView {
    pub idx: usize,
    pub name: String,
    pub task_id: Option<TaskId>,
    pub skipped: bool,
    pub task_state: Option<TaskState>,
    pub model_limit: u32,
    pub tool_limit: u32,
    pub model_used: u64,
    pub tool_used: u64,
}

#[derive(Debug, Clone)]
pub struct WorkflowRow {
    pub id: WorkflowId,
    pub name: String,
    pub approved: bool,
    pub cancel_requested: bool,
    pub failed: bool,
    pub created_ts: i64,
    pub steps: Vec<StepView>,
}

impl WorkflowRow {
    pub fn state(&self) -> WorkflowState {
        let steps: Vec<StepStatus> = self
            .steps
            .iter()
            .map(|s| StepStatus::of(s.task_state, s.skipped))
            .collect();
        state_of_steps(&steps, self.failed)
    }
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub seq: u64,
    pub event_type: String,
    pub ts: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum EventPage {
    Latest,
    Before(u64),
    After(u64),
}

/// Model requests and tool actions used, by task id.
type Usage = HashMap<String, (u64, u64)>;

fn corrupt<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> ViewError + '_ {
    move |e| ViewError::Corrupt(format!("{what}: {e}"))
}

fn id_of<T: serde::de::DeserializeOwned>(what: &str, s: String) -> Result<T> {
    serde_json::from_value(Value::String(s)).map_err(corrupt(what))
}

fn count<T: TryFrom<i64>>(what: &str, n: i64) -> Result<T> {
    T::try_from(n).map_err(|_| ViewError::Corrupt(format!("{what} {n} out of range")))
}

fn state_of(s: String) -> Result<TaskState> {
    state_from_str(s).map_err(corrupt("task state"))
}

impl View {
    pub fn open(path: &Path) -> Result<View> {
        let refused = |source| ViewError::Open {
            path: path.to_path_buf(),
            source,
        };
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ViewError::Missing(path.to_path_buf()));
            }
            Err(e) => return Err(refused(e)),
            Ok(m) if !m.is_file() => {
                return Err(refused(std::io::Error::other("not a regular file")));
            }
            Ok(_) => {}
        }
        // SQLite's open error only says it cannot open; a plain open says why.
        std::fs::File::open(path).map_err(refused)?;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "query_only", true)?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found != SCHEMA_VERSION {
            return Err(ViewError::Schema {
                found,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(View { conn })
    }

    fn read(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Deferred,
        )?)
    }

    /// The newest `limit` items, workflows and tasks outside any workflow together, read in
    /// one transaction so they agree. A step task is read through its workflow.
    pub fn snapshot(&self, limit: usize) -> Result<Snapshot> {
        let tx = self.read()?;
        let fetch = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut skipped = 0;
        let usage = usage(&tx, fetch)?;
        let (mut tasks, task_reads) = task_rows(&tx, fetch, &usage, &mut skipped)?;
        let (mut workflows, workflow_reads) = workflow_rows(&tx, fetch, &usage, &mut skipped)?;
        // Both are newest first: keep the newest `limit` of the two together.
        let (mut t, mut w) = (0, 0);
        while t + w < limit {
            match (tasks.get(t), workflows.get(w)) {
                (Some(task), Some(wf)) if task.created_ts >= wf.created_ts => t += 1,
                (_, Some(_)) => w += 1,
                (Some(_), None) => t += 1,
                (None, None) => break,
            }
        }
        let more =
            task_reads > limit || workflow_reads > limit || tasks.len() > t || workflows.len() > w;
        tasks.truncate(t);
        workflows.truncate(w);
        Ok(Snapshot {
            tasks,
            workflows,
            more,
            skipped,
        })
    }

    pub fn contract(&self, task: &TaskId) -> Result<Option<Contract>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT contract_json FROM tasks WHERE id = ?1",
                [task.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|j| serde_json::from_str(&j).map_err(corrupt("contract")))
            .transpose()
    }

    /// Events in ascending `seq` order.
    pub fn events(&self, task: &TaskId, page: EventPage, limit: usize) -> Result<Vec<EventRow>> {
        events_in(&self.conn, task, page, limit)
    }

    pub fn event_count(&self, task: &TaskId) -> Result<u64> {
        event_count_in(&self.conn, task)
    }

    /// `events` and the task's event count, read in one transaction so they agree.
    pub fn events_and_count(
        &self,
        task: &TaskId,
        page: EventPage,
        limit: usize,
    ) -> Result<(Vec<EventRow>, u64)> {
        let tx = self.read()?;
        let rows = events_in(&tx, task, page, limit)?;
        let total = event_count_in(&tx, task)?;
        Ok((rows, total))
    }

    /// The payload of event `seq` cut to `max_chars` characters, and whether it was cut.
    pub fn payload(
        &self,
        task: &TaskId,
        seq: u64,
        max_chars: usize,
    ) -> Result<Option<(String, bool)>> {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        let max = i64::try_from(max_chars).unwrap_or(i64::MAX);
        Ok(self
            .conn
            .query_row(
                "SELECT substr(payload, 1, ?3), length(payload) > ?3
                 FROM events WHERE task_id = ?1 AND seq = ?2",
                params![task.as_str(), seq, max],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?)
    }
}

/// Events in ascending `seq` order.
fn events_in(
    conn: &Connection,
    task: &TaskId,
    page: EventPage,
    limit: usize,
) -> Result<Vec<EventRow>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let (sql, bound) = match page {
        EventPage::Latest => (
            // `?3 = ?3` only uses the bound: rusqlite refuses a parameter the SQL never reads.
            "SELECT seq, type, ts FROM events WHERE task_id = ?1 AND ?3 = ?3
             ORDER BY seq DESC LIMIT ?2",
            0,
        ),
        EventPage::Before(s) => (
            "SELECT seq, type, ts FROM events WHERE task_id = ?1 AND seq < ?3
             ORDER BY seq DESC LIMIT ?2",
            i64::try_from(s).unwrap_or(i64::MAX),
        ),
        EventPage::After(s) => (
            "SELECT seq, type, ts FROM events WHERE task_id = ?1 AND seq > ?3
             ORDER BY seq ASC LIMIT ?2",
            i64::try_from(s).unwrap_or(i64::MAX),
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let raw = stmt
        .query_map(params![task.as_str(), limit, bound], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows = raw
        .into_iter()
        .map(|(seq, event_type, ts)| {
            Ok(EventRow {
                seq: count("event seq", seq)?,
                event_type,
                ts,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !matches!(page, EventPage::After(_)) {
        rows.reverse();
    }
    Ok(rows)
}

fn event_count_in(conn: &Connection, task: &TaskId) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE task_id = ?1",
        [task.as_str()],
        |r| r.get(0),
    )?;
    count("event count", n)
}

#[cfg(test)]
thread_local! {
    /// Statements a snapshot prepared on this thread; tests read it to bound the query count.
    static PREPARED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn prepare<'a>(tx: &'a Transaction, sql: &str) -> Result<rusqlite::Statement<'a>> {
    #[cfg(test)]
    PREPARED.with(|n| n.set(n.get() + 1));
    Ok(tx.prepare(sql)?)
}

/// The newest `?1` tasks outside any workflow, as `task_rows` reads them.
const TASK_WINDOW: &str = "SELECT t.id FROM tasks t
     WHERE NOT EXISTS (SELECT 1 FROM workflow_steps ws WHERE ws.task_id = t.id)
     ORDER BY t.created_ts DESC, t.rowid DESC LIMIT ?1";
/// The newest `?1` workflows, as `workflow_rows` reads them.
const WORKFLOW_WINDOW: &str =
    "SELECT id FROM workflows ORDER BY created_ts DESC, rowid DESC LIMIT ?1";

/// Per-task usage the way `usage_totals` in `effects.rs` counts it: settled rows count what
/// they settled, released rows nothing, every other row what it reserved. Only the tasks a
/// snapshot of `fetch` rows can show are counted: the windowed tasks and the steps of the
/// windowed workflows.
fn usage(tx: &Transaction, fetch: i64) -> Result<Usage> {
    let mut stmt = prepare(
        tx,
        &format!(
            "SELECT task_id,
                SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_model_requests, 0)
                         WHEN status = 'Released' THEN 0 ELSE reserved_model_requests END),
                SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_tool_actions, 0)
                         WHEN status = 'Released' THEN 0 ELSE reserved_tool_actions END)
             FROM usage
             WHERE task_id IN ({TASK_WINDOW})
                OR task_id IN (SELECT task_id FROM workflow_steps
                               WHERE workflow_id IN ({WORKFLOW_WINDOW}))
             GROUP BY task_id"
        ),
    )?;
    let raw = stmt
        .query_map([fetch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    raw.into_iter()
        .map(|(task, m, a)| Ok((task, (count("model usage", m)?, count("tool usage", a)?))))
        .collect()
}

/// Rows that parse, keeping count of those that do not.
fn readable<T>(rows: impl Iterator<Item = Result<T>>, skipped: &mut usize) -> Vec<T> {
    rows.filter_map(|row| match row {
        Ok(row) => Some(row),
        Err(_) => {
            *skipped += 1;
            None
        }
    })
    .collect()
}

/// The newest `fetch` tasks outside any workflow, and how many rows were read.
fn task_rows(
    tx: &Transaction,
    fetch: i64,
    usage: &Usage,
    skipped: &mut usize,
) -> Result<(Vec<TaskRow>, usize)> {
    // The same window as `TASK_WINDOW`, with the columns a row needs.
    let mut stmt = prepare(
        tx,
        "SELECT t.id, t.state, t.cancel_requested, t.contract_json, t.created_ts
         FROM tasks t
         WHERE NOT EXISTS (SELECT 1 FROM workflow_steps ws WHERE ws.task_id = t.id)
         ORDER BY t.created_ts DESC, t.rowid DESC LIMIT ?1",
    )?;
    let raw = stmt
        .query_map([fetch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let reads = raw.len();
    let rows = raw
        .into_iter()
        .map(|(id, state, cancel_requested, contract, created_ts)| {
            let contract: Contract =
                serde_json::from_str(&contract).map_err(corrupt("contract"))?;
            let (model_used, tool_used) = usage.get(&id).copied().unwrap_or_default();
            Ok(TaskRow {
                id: id_of("task id", id)?,
                state: state_of(state)?,
                cancel_requested,
                goal: contract.goal,
                model_limit: contract.limits.model_requests,
                tool_limit: contract.limits.tool_actions,
                model_used,
                tool_used,
                created_ts,
            })
        });
    Ok((readable(rows, skipped), reads))
}

/// One `workflow_steps` row joined to its task's state.
type StepRaw = (i64, String, Option<String>, bool, Option<String>);

/// The newest `fetch` workflows with their steps, and how many workflow rows were read.
fn workflow_rows(
    tx: &Transaction,
    fetch: i64,
    usage: &Usage,
    skipped: &mut usize,
) -> Result<(Vec<WorkflowRow>, usize)> {
    let mut stmt = prepare(
        tx,
        "SELECT id, spec_json, failure IS NOT NULL, approved_ts IS NOT NULL, cancel_requested,
            created_ts
         FROM workflows ORDER BY created_ts DESC, rowid DESC LIMIT ?1",
    )?;
    let raw = stmt
        .query_map([fetch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // Every step of every windowed workflow in one read, grouped here.
    let mut stmt = prepare(
        tx,
        &format!(
            "SELECT ws.workflow_id, ws.idx, ws.name, ws.task_id, ws.skipped, t.state
             FROM workflow_steps ws
             LEFT JOIN tasks t ON t.id = ws.task_id
             WHERE ws.workflow_id IN ({WORKFLOW_WINDOW})
             ORDER BY ws.workflow_id, ws.idx"
        ),
    )?;
    let mut steps: HashMap<String, Vec<StepRaw>> = HashMap::new();
    for row in stmt.query_map([fetch], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, Option<String>>(5)?,
            ),
        ))
    })? {
        let (workflow, step) = row?;
        steps.entry(workflow).or_default().push(step);
    }
    let reads = raw.len();
    let rows = raw.into_iter().map(
        |(id, spec, failed, approved, cancel_requested, created_ts)| {
            let spec: ChainSpec = serde_json::from_str(&spec).map_err(corrupt("workflow spec"))?;
            let steps = step_views(&id, &spec, steps.remove(&id).unwrap_or_default(), usage)?;
            Ok(WorkflowRow {
                id: id_of("workflow id", id)?,
                name: spec.name,
                approved,
                cancel_requested,
                failed,
                created_ts,
                steps,
            })
        },
    );
    Ok((readable(rows, skipped), reads))
}

fn step_views(
    workflow: &str,
    spec: &ChainSpec,
    raw: Vec<StepRaw>,
    usage: &Usage,
) -> Result<Vec<StepView>> {
    raw.into_iter()
        .map(|(idx, name, task, skipped, state)| {
            let idx: usize = count("step index", idx)?;
            let limits = &spec
                .steps
                .get(idx)
                .ok_or_else(|| {
                    ViewError::Corrupt(format!("workflow {workflow} has no step {idx} in its spec"))
                })?
                .contract
                .limits;
            let (model_used, tool_used) = task
                .as_ref()
                .and_then(|t| usage.get(t).copied())
                .unwrap_or_default();
            Ok(StepView {
                idx,
                name,
                task_id: task.map(|t| id_of("task id", t)).transpose()?,
                skipped,
                task_state: state.map(state_of).transpose()?,
                model_limit: limits.model_requests,
                tool_limit: limits.tool_actions,
                model_used,
                tool_used,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    use agentos_core::contract::Contract;
    use agentos_core::ids::Digest;

    pub(crate) fn contract() -> Contract {
        Contract::parse(CONTRACT_JSON).unwrap()
    }
    pub(crate) fn digest() -> Digest {
        Digest::of(CONTRACT_JSON.as_bytes())
    }
    const CONTRACT_JSON: &str = r#"{
    "goal": "fix the parser",
    "repository": {"source": "fixtures/parser-repo", "revision": "abc123"},
    "profile": "protected", "editable_paths": ["src/"],
    "verification_profile": "parser-checks-v1",
    "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run"],
    "limits": {"model_requests": 10, "max_output_tokens_per_request": 1000, "tool_actions": 1000,
               "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256}
}"#;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn home() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentos.db");
        (dir, path)
    }

    #[test]
    fn a_missing_database_is_refused_and_not_created() {
        let (_dir, path) = home();
        assert!(matches!(View::open(&path), Err(ViewError::Missing(p)) if p == path));
        assert!(!path.exists());
        assert!(!path.with_extension("db-wal").exists());
    }

    #[test]
    fn another_schema_version_is_refused_and_left_alone() {
        let (_dir, path) = home();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch("CREATE TABLE tasks(id TEXT);").unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            View::open(&path),
            Err(ViewError::Schema {
                found: 2,
                expected: 3
            })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before, "no migration");
    }

    #[test]
    fn no_write_succeeds_through_the_view_connection() {
        let (_dir, path) = home();
        Db::open(&path).unwrap();
        let view = View::open(&path).unwrap();
        for sql in [
            "INSERT INTO workflows(id, owner, spec_digest, spec_json, options_json, created_ts) VALUES('x','o','d','{}','{}',0)",
            "DELETE FROM tasks",
            "CREATE TABLE evil(a)",
            "PRAGMA user_version = 9",
        ] {
            assert!(view.conn.execute_batch(sql).is_err(), "{sql} must fail");
        }
    }

    #[test]
    fn a_write_made_after_the_open_is_visible() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let view = View::open(&path).unwrap();
        assert!(view.snapshot(10).unwrap().tasks.is_empty());
        db.create_task(
            &crate::view::testing::contract(),
            &crate::view::testing::digest(),
        )
        .unwrap();
        assert_eq!(view.snapshot(10).unwrap().tasks.len(), 1);
    }

    #[test]
    fn the_newest_rows_come_first_and_more_says_when_there_are_others() {
        let (_dir, path) = home();
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(100));
        let t = ticks.clone();
        let db = Db::open(&path).unwrap().with_clock(Box::new(move || {
            t.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }));
        let ids: Vec<_> = (0..3)
            .map(|_| {
                db.create_task(
                    &crate::view::testing::contract(),
                    &crate::view::testing::digest(),
                )
                .unwrap()
            })
            .collect();
        let snap = View::open(&path).unwrap().snapshot(2).unwrap();
        assert_eq!(
            snap.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(),
            vec![&ids[2], &ids[1]]
        );
        assert!(snap.more);
        assert!(!View::open(&path).unwrap().snapshot(3).unwrap().more);
    }

    fn spec(n: usize) -> ChainSpec {
        let contract = serde_json::json!({
            "goal": "do it", "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        });
        let steps: Vec<_> = (0..n)
            .map(|i| serde_json::json!({ "name": format!("step-{i}"), "contract": contract }))
            .collect();
        ChainSpec::parse(
            &serde_json::json!({
                "name": "chain",
                "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
                "steps": steps
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn a_step_task_carries_its_workflow_and_a_failed_chain_says_so() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let s = spec(2);
        let id = db.create_workflow(&s, &serde_json::json!({})).unwrap();
        let view = View::open(&path).unwrap();
        let snap = view.snapshot(10).unwrap();
        assert!(!snap.workflows[0].approved);
        assert_eq!(snap.workflows[0].state(), WorkflowState::Running);
        db.approve_workflow(&id).unwrap();
        let task = db
            .create_step_task(&id, 0, &s.steps[0].contract, &s.digest())
            .unwrap();
        let snap = view.snapshot(10).unwrap();
        assert!(
            snap.tasks.is_empty(),
            "a step task is read through its workflow"
        );
        let w = &snap.workflows[0];
        assert!(w.approved);
        assert_eq!(w.steps[0].task_id.as_ref(), Some(&task));
        assert!(w.steps[0].task_state.is_some() && w.steps[1].task_state.is_none());
        assert_eq!((w.steps[0].model_limit, w.steps[0].tool_limit), (3, 4));
        assert_eq!(w.state(), WorkflowState::Running);
        db.fail_workflow(&id, "boom").unwrap();
        let w = &view.snapshot(10).unwrap().workflows[0];
        assert!(w.failed);
        assert_eq!(w.state(), WorkflowState::Failed { step: 1 });
    }

    #[test]
    fn usage_counts_settled_reserved_and_released_like_the_budget_does() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let id = db
            .create_task(
                &crate::view::testing::contract(),
                &crate::view::testing::digest(),
            )
            .unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            for (status, reserved, settled) in [
                ("Settled", (3, 4), Some((2, 3))),
                ("Reserved", (1, 1), None),
                ("Released", (5, 5), None),
            ] {
                conn.execute(
                    "INSERT INTO usage(task_id, kind, reserved_model_requests, reserved_tool_actions,
                                       settled_model_requests, settled_tool_actions, status)
                     VALUES(?1, 'k', ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id.as_str(), reserved.0, reserved.1,
                        settled.map(|s| s.0), settled.map(|s| s.1), status
                    ],
                )
                .unwrap();
            }
        }
        let task = &View::open(&path).unwrap().snapshot(10).unwrap().tasks[0];
        assert_eq!((task.model_used, task.tool_used), (3, 4)); // 2 settled + 1 reserved; 3 + 1
    }

    #[test]
    fn events_page_in_both_directions_and_payloads_are_capped() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let id = db
            .create_task(
                &crate::view::testing::contract(),
                &crate::view::testing::digest(),
            )
            .unwrap();
        for i in 0..5 {
            db.append_audit(
                &id,
                "Note",
                &serde_json::json!({ "i": i, "pad": "x".repeat(100) }),
            )
            .unwrap();
        }
        let view = View::open(&path).unwrap();
        let total = view.event_count(&id).unwrap();
        let (page, counted) = view.events_and_count(&id, EventPage::Latest, 2).unwrap();
        assert_eq!(counted, total);
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [total - 1, total]
        );
        let latest = view.events(&id, EventPage::Latest, 2).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[1].seq, total, "ascending, ending at the newest");
        let before = view
            .events(&id, EventPage::Before(latest[0].seq), 2)
            .unwrap();
        assert!(before.iter().all(|e| e.seq < latest[0].seq));
        assert!(before.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(
            view.events(&id, EventPage::After(total), 10)
                .unwrap()
                .is_empty()
        );
        let (text, truncated) = view.payload(&id, total, 10).unwrap().unwrap();
        assert_eq!(text.chars().count(), 10);
        assert!(truncated);
        assert!(view.payload(&id, total + 1, 10).unwrap().is_none());
    }

    /// Rows written straight into the tables in one transaction, each with its own created time.
    struct Seed {
        conn: rusqlite::Connection,
    }

    impl Seed {
        fn new(path: &Path) -> Seed {
            Db::open(path).unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("BEGIN").unwrap();
            Seed { conn }
        }

        fn task(&self, ts: i64) -> String {
            let id = TaskId::new().to_string();
            self.conn
                .execute(
                    "INSERT INTO tasks(id, owner, contract_digest, contract_json, state,
                        cancel_requested, workspace_digest, actions_used, step, deadline_ts,
                        created_ts)
                     VALUES(?1, 'o', 'd', ?2, 'Running', 0, 'w', 0, 0, 0, ?3)",
                    params![id, serde_json::to_string(&testing::contract()).unwrap(), ts],
                )
                .unwrap();
            id
        }

        /// A two-step workflow; with `tasks`, each step has a task created just after it.
        fn workflow(&self, ts: i64, tasks: bool) -> String {
            let id = WorkflowId::new().to_string();
            self.conn
                .execute(
                    "INSERT INTO workflows(id, owner, spec_digest, spec_json, options_json,
                        approved_ts, created_ts)
                     VALUES(?1, 'o', 'd', ?2, '{}', ?3, ?3)",
                    params![id, serde_json::to_string(&spec(2)).unwrap(), ts],
                )
                .unwrap();
            for idx in 0..2 {
                let task = tasks.then(|| self.task(ts + 1 + idx));
                self.conn
                    .execute(
                        "INSERT INTO workflow_steps(workflow_id, idx, name, task_id)
                         VALUES(?1, ?2, ?3, ?4)",
                        params![id, idx, format!("step-{idx}"), task],
                    )
                    .unwrap();
            }
            id
        }

        fn done(self) {
            self.conn.execute_batch("COMMIT").unwrap();
        }
    }

    #[test]
    fn usage_is_counted_only_for_the_tasks_a_snapshot_can_show() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        let old = seed.task(0);
        let old_flow = seed.workflow(10, true);
        let new = seed.task(100);
        let new_flow = seed.workflow(110, true);
        let step_tasks = |flow: &str| -> Vec<String> {
            let mut stmt = seed
                .conn
                .prepare("SELECT task_id FROM workflow_steps WHERE workflow_id = ?1")
                .unwrap();
            stmt.query_map([flow], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        let (old_steps, new_steps) = (step_tasks(&old_flow), step_tasks(&new_flow));
        for task in [&old, &new].into_iter().chain(&old_steps).chain(&new_steps) {
            seed.conn
                .execute(
                    "INSERT INTO usage(task_id, kind, reserved_model_requests,
                        reserved_tool_actions, status)
                     VALUES(?1, 'k', 1, 1, 'Reserved')",
                    [task],
                )
                .unwrap();
        }
        seed.done();
        let view = View::open(&path).unwrap();
        let tx = view.read().unwrap();

        let counted = usage(&tx, 1).unwrap();

        let mut got: Vec<_> = counted.keys().cloned().collect();
        got.sort();
        let mut want = vec![new.clone()];
        want.extend(new_steps.iter().cloned());
        want.sort();
        assert_eq!(
            got, want,
            "only the newest task and the newest workflow's steps"
        );
        assert!(!counted.contains_key(&old));
        assert!(old_steps.iter().all(|t| !counted.contains_key(t)));
    }

    #[test]
    fn a_failed_workflow_reads_as_failed_from_its_failure_column() {
        let (_dir, path) = home();
        let db = Db::open(&path).unwrap();
        let s = spec(2);
        let id = db
            .create_workflow(&s, &serde_json::json!({ "worker": "host" }))
            .unwrap();
        db.approve_workflow(&id).unwrap();
        db.fail_workflow(&id, "handoff result mismatch").unwrap();

        let snap = View::open(&path).unwrap().snapshot(10).unwrap();

        assert_eq!(snap.workflows.len(), 1);
        assert!(snap.workflows[0].failed);
        assert_eq!(snap.workflows[0].state(), WorkflowState::Failed { step: 0 });
    }

    #[test]
    fn step_tasks_do_not_crowd_loose_tasks_out_of_the_window() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        let loose: Vec<_> = (0..10).map(|i| seed.task(i)).collect();
        for i in 0..300 {
            seed.workflow(100 + 10 * i, true);
        }
        seed.done();
        let snap = View::open(&path).unwrap().snapshot(500).unwrap();
        let got: Vec<_> = snap.tasks.iter().map(|t| t.id.to_string()).collect();
        assert_eq!(got.len(), 10, "no step task is a row");
        assert!(loose.iter().all(|l| got.contains(l)), "every loose task");
        assert_eq!(snap.workflows.len(), 300);
        assert!(!snap.more, "310 items fit in 500");
        assert!(snap.workflows.iter().all(|w| {
            w.steps.len() == 2
                && w.steps
                    .iter()
                    .all(|s| s.task_state == Some(TaskState::Running))
        }));
    }

    #[test]
    fn a_step_task_whose_workflow_is_outside_the_window_is_not_a_row() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        seed.workflow(1, true); // its step tasks are created at 2 and 3
        let newest = seed.workflow(10, false);
        seed.done();
        let snap = View::open(&path).unwrap().snapshot(1).unwrap();
        assert!(snap.tasks.is_empty(), "{:?}", snap.tasks);
        assert_eq!(
            snap.workflows
                .iter()
                .map(|w| w.id.to_string())
                .collect::<Vec<_>>(),
            vec![newest]
        );
        assert!(snap.more);
    }

    #[test]
    fn the_merged_list_holds_at_most_limit_items_the_newest_overall() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        for i in 0..500 {
            seed.task(2 * i);
            seed.workflow(2 * i + 1, false);
        }
        seed.done();
        let snap = View::open(&path).unwrap().snapshot(500).unwrap();
        assert_eq!(snap.tasks.len() + snap.workflows.len(), 500);
        assert!(snap.more);
        let oldest_kept = snap
            .tasks
            .iter()
            .map(|t| t.created_ts)
            .chain(snap.workflows.iter().map(|w| w.created_ts))
            .min()
            .unwrap();
        assert_eq!(oldest_kept, 500, "times 500..=999 are the newest 500");
        let exact = View::open(&path).unwrap().snapshot(1000).unwrap();
        assert_eq!(exact.tasks.len() + exact.workflows.len(), 1000);
        assert!(!exact.more);
    }

    #[test]
    fn a_snapshot_runs_a_fixed_number_of_statements_however_many_workflows() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        for i in 0..200 {
            seed.workflow(10 * i, true);
        }
        seed.done();
        let view = View::open(&path).unwrap();
        PREPARED.with(|n| n.set(0));
        let snap = view.snapshot(500).unwrap();
        assert_eq!(snap.workflows.len(), 200);
        // the usage aggregate, the loose tasks, the workflows and all their steps
        assert_eq!(PREPARED.with(|n| n.get()), 4);
    }

    #[test]
    fn a_row_that_cannot_be_read_is_skipped_and_counted() {
        let (_dir, path) = home();
        let seed = Seed::new(&path);
        let good = seed.task(1);
        let bad_contract = seed.task(2);
        let bad_state = seed.task(3);
        let good_wf = seed.workflow(4, true);
        let bad_spec = seed.workflow(10, false);
        let bad_step = seed.workflow(20, true);
        let conn = &seed.conn;
        conn.execute(
            "UPDATE tasks SET contract_json = 'not json' WHERE id = ?1",
            [&bad_contract],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET state = 'Exploded' WHERE id = ?1",
            [&bad_state],
        )
        .unwrap();
        conn.execute(
            "UPDATE workflows SET spec_json = '{' WHERE id = ?1",
            [&bad_spec],
        )
        .unwrap();
        conn.execute(
            "UPDATE workflow_steps SET idx = -1 WHERE workflow_id = ?1 AND idx = 1",
            [&bad_step],
        )
        .unwrap();
        seed.done();
        let snap = View::open(&path).unwrap().snapshot(500).unwrap();
        assert_eq!(
            snap.tasks
                .iter()
                .map(|t| t.id.to_string())
                .collect::<Vec<_>>(),
            vec![good]
        );
        assert_eq!(
            snap.workflows
                .iter()
                .map(|w| w.id.to_string())
                .collect::<Vec<_>>(),
            vec![good_wf]
        );
        assert_eq!(snap.skipped, 4);
    }

    #[test]
    fn a_database_that_exists_but_cannot_be_opened_is_an_open_error_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        // the home is a regular file: the path below it is not "missing", it cannot be reached
        let home = dir.path().join("home");
        std::fs::write(&home, "").unwrap();
        let path = home.join("agentos.db");
        let err = View::open(&path).err().expect("refused");
        assert!(
            matches!(&err, ViewError::Open { path: p, .. } if p == &path),
            "{err}"
        );
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        // a directory where the database should be
        let path = dir.path().join("agentos.db");
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(View::open(&path), Err(ViewError::Open { .. })));
    }

    #[test]
    fn an_unreadable_database_is_an_open_error() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = home();
        Db::open(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            eprintln!("skipped: permissions do not bind this user (root)");
            return;
        }
        let err = View::open(&path).err().expect("refused");
        assert!(matches!(&err, ViewError::Open { .. }), "{err}");
        assert!(err.to_string().contains("ermission denied"), "{err}");
    }
}
