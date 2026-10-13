//! A read-only window onto the store for the cockpit. `View` is a separate type from `Db`
//! so that nothing reachable from it can write.

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
    pub workflow: Option<(WorkflowId, usize)>,
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
            .map(|s| match (s.task_state, s.skipped) {
                (Some(state), _) => StepStatus::Task(state),
                (None, true) => StepStatus::Skipped,
                (None, false) => StepStatus::Pending,
            })
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

/// Per-task usage the way `usage_totals` in `effects.rs` counts it: settled rows count what
/// they settled, released rows nothing, every other row what it reserved.
const USAGE: &str = "(SELECT task_id,
        SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_model_requests, 0)
                 WHEN status = 'Released' THEN 0 ELSE reserved_model_requests END) AS m,
        SUM(CASE WHEN status = 'Settled' THEN COALESCE(settled_tool_actions, 0)
                 WHEN status = 'Released' THEN 0 ELSE reserved_tool_actions END) AS a
      FROM usage GROUP BY task_id)";

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
        if !path.is_file() {
            return Err(ViewError::Missing(path.to_path_buf()));
        }
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

    /// The newest `limit` tasks and workflows, read in one transaction so they agree.
    pub fn snapshot(&self, limit: usize) -> Result<Snapshot> {
        let tx = self.read()?;
        let fetch = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut tasks = task_rows(&tx, fetch)?;
        let mut workflows = workflow_rows(&tx, fetch)?;
        let more = tasks.len() > limit || workflows.len() > limit;
        tasks.truncate(limit);
        workflows.truncate(limit);
        Ok(Snapshot {
            tasks,
            workflows,
            more,
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
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (sql, bound) = match page {
            EventPage::Latest => (
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
        let mut stmt = self.conn.prepare(sql)?;
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

    pub fn event_count(&self, task: &TaskId) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1",
            [task.as_str()],
            |r| r.get(0),
        )?;
        count("event count", n)
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

fn task_rows(tx: &Transaction, fetch: i64) -> Result<Vec<TaskRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT t.id, t.state, t.cancel_requested, t.contract_json, t.created_ts,
                COALESCE(u.m, 0), COALESCE(u.a, 0), ws.workflow_id, ws.idx
         FROM tasks t
         LEFT JOIN {USAGE} u ON u.task_id = t.id
         LEFT JOIN workflow_steps ws ON ws.task_id = t.id
         ORDER BY t.created_ts DESC, t.rowid DESC LIMIT ?1"
    ))?;
    let raw = stmt
        .query_map([fetch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    raw.into_iter()
        .map(
            |(id, state, cancel_requested, contract, created_ts, m, a, wf, idx)| {
                let contract: Contract =
                    serde_json::from_str(&contract).map_err(corrupt("contract"))?;
                let workflow = match (wf, idx) {
                    (Some(wf), Some(idx)) => {
                        Some((id_of("workflow id", wf)?, count("step index", idx)?))
                    }
                    _ => None,
                };
                Ok(TaskRow {
                    id: id_of("task id", id)?,
                    state: state_of(state)?,
                    cancel_requested,
                    goal: contract.goal,
                    model_limit: contract.limits.model_requests,
                    tool_limit: contract.limits.tool_actions,
                    model_used: count("model usage", m)?,
                    tool_used: count("tool usage", a)?,
                    created_ts,
                    workflow,
                })
            },
        )
        .collect()
}

fn workflow_rows(tx: &Transaction, fetch: i64) -> Result<Vec<WorkflowRow>> {
    let mut stmt = tx.prepare(
        "SELECT id, spec_json, options_json, approved_ts IS NOT NULL, cancel_requested, created_ts
         FROM workflows ORDER BY created_ts DESC, rowid DESC LIMIT ?1",
    )?;
    let raw = stmt
        .query_map([fetch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    raw.into_iter()
        .map(
            |(id, spec, options, approved, cancel_requested, created_ts)| {
                let spec: ChainSpec =
                    serde_json::from_str(&spec).map_err(corrupt("workflow spec"))?;
                let options: Value =
                    serde_json::from_str(&options).map_err(corrupt("workflow options"))?;
                let steps = step_views(tx, &id, &spec)?;
                Ok(WorkflowRow {
                    id: id_of("workflow id", id)?,
                    name: spec.name,
                    approved,
                    cancel_requested,
                    failed: options.get("failure").is_some(),
                    created_ts,
                    steps,
                })
            },
        )
        .collect()
}

fn step_views(tx: &Transaction, workflow: &str, spec: &ChainSpec) -> Result<Vec<StepView>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT ws.idx, ws.name, ws.task_id, ws.skipped, t.state, COALESCE(u.m, 0), COALESCE(u.a, 0)
         FROM workflow_steps ws
         LEFT JOIN tasks t ON t.id = ws.task_id
         LEFT JOIN {USAGE} u ON u.task_id = ws.task_id
         WHERE ws.workflow_id = ?1 ORDER BY ws.idx"
    ))?;
    let raw = stmt
        .query_map([workflow], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    raw.into_iter()
        .map(|(idx, name, task, skipped, state, m, a)| {
            let idx: usize = count("step index", idx)?;
            let limits = &spec
                .steps
                .get(idx)
                .ok_or_else(|| {
                    ViewError::Corrupt(format!("workflow {workflow} has no step {idx} in its spec"))
                })?
                .contract
                .limits;
            Ok(StepView {
                idx,
                name,
                task_id: task.map(|t| id_of("task id", t)).transpose()?,
                skipped,
                task_state: state.map(state_of).transpose()?,
                model_limit: limits.model_requests,
                tool_limit: limits.tool_actions,
                model_used: count("model usage", m)?,
                tool_used: count("tool usage", a)?,
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
        assert_eq!(snap.tasks[0].workflow, Some((id.clone(), 0)));
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
}
