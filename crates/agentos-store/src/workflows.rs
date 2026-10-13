//! Workflows: a chain of task contracts run one after the other. The store holds the chain,
//! the owner's approval and each step's task, and enforces the order in which step tasks may
//! be created; what a step's task does is the engine's business.

use agentos_core::contract::Contract;
use agentos_core::ids::{Digest, TaskId, WorkflowId};
use agentos_core::state::TaskState;
use agentos_core::workflow::ChainSpec;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::db::{Db, DbError, OWNER, Result, digest_from_str, insert_task, now_ts, state_to_str};

#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub idx: usize,
    pub name: String,
    pub task_id: Option<TaskId>,
    pub skipped: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRecord {
    pub id: WorkflowId,
    pub spec: ChainSpec,
    pub spec_digest: Digest,
    pub options: Value,
    pub approved: bool,
    pub cancel_requested: bool,
    pub steps: Vec<StepRow>,
}

fn refuse<T>(msg: impl Into<String>) -> Result<T> {
    Err(DbError::Workflow(msg.into()))
}

/// `(approved, cancel_requested)`, or refuses an unknown workflow.
fn flags(tx: &Transaction, id: &WorkflowId) -> Result<(bool, bool)> {
    tx.query_row(
        "SELECT approved_ts IS NOT NULL, cancel_requested FROM workflows WHERE id = ?1",
        [id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .map_or_else(|| refuse(format!("workflow {id} not found")), Ok)
}

fn step_row(tx: &Transaction, id: &WorkflowId, idx: usize) -> Result<(Option<TaskId>, bool)> {
    let row = tx
        .query_row(
            "SELECT task_id, skipped FROM workflow_steps WHERE workflow_id = ?1 AND idx = ?2",
            params![id.as_str(), idx as i64],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, bool>(1)?)),
        )
        .optional()?;
    match row {
        None => refuse(format!("workflow {id} has no step {idx}")),
        Some((task, skipped)) => Ok((task.map(task_id).transpose()?, skipped)),
    }
}

fn task_id(s: String) -> Result<TaskId> {
    Ok(serde_json::from_value(Value::String(s))?)
}

impl Db {
    /// Stores a chain and one pending step per step of it. Nothing runs until the owner
    /// approves it (`approve_workflow`).
    pub fn create_workflow(&self, spec: &ChainSpec, options: &Value) -> Result<WorkflowId> {
        let id = WorkflowId::new();
        let tx = self.immediate()?;
        tx.execute(
            "INSERT INTO workflows(id, owner, spec_digest, spec_json, options_json, approved_ts,
                cancel_requested, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 0, ?6)",
            params![
                id.as_str(),
                OWNER,
                spec.digest().to_string(),
                serde_json::to_string(spec)?,
                serde_json::to_string(options)?,
                now_ts()
            ],
        )?;
        for (idx, step) in spec.steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO workflow_steps(workflow_id, idx, name) VALUES (?1, ?2, ?3)",
                params![id.as_str(), idx as i64, step.name],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Records the owner's approval of the whole chain. The first approval's time stands.
    pub fn approve_workflow(&self, id: &WorkflowId) -> Result<()> {
        let tx = self.immediate()?;
        flags(&tx, id)?;
        tx.execute(
            "UPDATE workflows SET approved_ts = ?2 WHERE id = ?1 AND approved_ts IS NULL",
            params![id.as_str(), now_ts()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn workflow(&self, id: &WorkflowId) -> Result<WorkflowRecord> {
        let tx = self.read()?;
        let row = tx
            .query_row(
                "SELECT spec_digest, spec_json, options_json, approved_ts IS NOT NULL, cancel_requested
                 FROM workflows WHERE id = ?1",
                [id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((digest, spec, options, approved, cancel_requested)) = row else {
            return refuse(format!("workflow {id} not found"));
        };
        let mut stmt = tx.prepare(
            "SELECT idx, name, task_id, skipped FROM workflow_steps WHERE workflow_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([id.as_str()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        let mut steps = Vec::new();
        for row in rows {
            let (idx, name, task, skipped) = row?;
            steps.push(StepRow {
                idx: idx as usize,
                name,
                task_id: task.map(task_id).transpose()?,
                skipped,
            });
        }
        Ok(WorkflowRecord {
            id: id.clone(),
            spec: serde_json::from_str(&spec)?,
            spec_digest: digest_from_str(&digest)?,
            options: serde_json::from_str(&options)?,
            approved,
            cancel_requested,
            steps,
        })
    }

    /// Creates the task of step `idx`, exactly as `create_task` would, and links it. Refused
    /// unless the chain is approved and not cancelled, the step is still untouched, and the
    /// previous step's task has succeeded.
    pub fn create_step_task(
        &self,
        id: &WorkflowId,
        idx: usize,
        contract: &Contract,
        contract_digest: &Digest,
    ) -> Result<TaskId> {
        let tx = self.immediate()?;
        let (approved, cancel_requested) = flags(&tx, id)?;
        if !approved {
            return refuse(format!("workflow {id} is not approved"));
        }
        if cancel_requested {
            return refuse(format!("workflow {id} is being cancelled"));
        }
        let (task, skipped) = step_row(&tx, id, idx)?;
        if task.is_some() {
            return refuse(format!("step {idx} of workflow {id} already has a task"));
        }
        if skipped {
            return refuse(format!("step {idx} of workflow {id} was skipped"));
        }
        if idx > 0 {
            let (prev, _) = step_row(&tx, id, idx - 1)?;
            let Some(prev) = prev else {
                return refuse(format!("step {} of workflow {id} has no task yet", idx - 1));
            };
            let state: String = tx.query_row(
                "SELECT state FROM tasks WHERE id = ?1",
                [prev.as_str()],
                |r| r.get(0),
            )?;
            if state != state_to_str(TaskState::Succeeded)? {
                return refuse(format!(
                    "step {} of workflow {id} has not succeeded (state {state})",
                    idx - 1
                ));
            }
        }
        let task = insert_task(&tx, contract, contract_digest)?;
        tx.execute(
            "UPDATE workflow_steps SET task_id = ?3 WHERE workflow_id = ?1 AND idx = ?2",
            params![id.as_str(), idx as i64, task.as_str()],
        )?;
        tx.commit()?;
        Ok(task)
    }

    /// Stops the chain from starting more steps: steps without a task are skipped for good.
    /// Running tasks are not touched here; cancelling them is the engine's job.
    pub fn request_workflow_cancel(&self, id: &WorkflowId) -> Result<()> {
        let tx = self.immediate()?;
        flags(&tx, id)?;
        tx.execute(
            "UPDATE workflows SET cancel_requested = 1 WHERE id = ?1",
            [id.as_str()],
        )?;
        tx.execute(
            "UPDATE workflow_steps SET skipped = 1 WHERE workflow_id = ?1 AND task_id IS NULL",
            [id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fails the chain where no step task can carry the failure (the handoff to the next step
    /// could not be built): steps without a task are skipped for good, and `reason` is kept as
    /// the options' `failure`. The first reason stands.
    pub fn fail_workflow(&self, id: &WorkflowId, reason: &str) -> Result<()> {
        let tx = self.immediate()?;
        flags(&tx, id)?;
        let options: String = tx.query_row(
            "SELECT options_json FROM workflows WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )?;
        let mut options: Value = serde_json::from_str(&options)?;
        let Some(fields) = options.as_object_mut() else {
            return refuse(format!("workflow {id} has options that are not an object"));
        };
        fields
            .entry("failure")
            .or_insert_with(|| Value::String(reason.to_string()));
        tx.execute(
            "UPDATE workflows SET options_json = ?2 WHERE id = ?1",
            params![id.as_str(), serde_json::to_string(&options)?],
        )?;
        tx.execute(
            "UPDATE workflow_steps SET skipped = 1 WHERE workflow_id = ?1 AND task_id IS NULL",
            [id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agentos_core::state::{TaskEvent, TaskState};
    use agentos_core::workflow::ChainSpec;
    use rusqlite::{Connection, params};
    use serde_json::json;

    use crate::db::{Db, DbError, SCHEMA_VERSION, state_to_str};

    fn spec(n: usize) -> ChainSpec {
        let contract = json!({
            "goal": "do it", "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        });
        let steps: Vec<_> = (0..n)
            .map(|i| json!({ "name": format!("step-{i}"), "contract": contract }))
            .collect();
        ChainSpec::parse(
            &json!({
                "name": "chain",
                "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
                "steps": steps
            })
            .to_string(),
        )
        .unwrap()
    }

    fn open(dir: &tempfile::TempDir) -> Db {
        Db::open(&dir.path().join("agentos.db")).unwrap()
    }

    fn task_count(db: &Db) -> i64 {
        let tx = db.immediate().unwrap();
        tx.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap()
    }

    fn set_state(db: &Db, task: &agentos_core::ids::TaskId, state: TaskState) {
        let tx = db.immediate().unwrap();
        tx.execute(
            "UPDATE tasks SET state = ?2 WHERE id = ?1",
            params![task.as_str(), state_to_str(state).unwrap()],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn refused(r: crate::db::Result<impl std::fmt::Debug>) -> String {
        match r {
            Err(DbError::Workflow(m)) => m,
            other => panic!("expected DbError::Workflow, got {other:?}"),
        }
    }

    #[test]
    fn create_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(3);
        let options = json!({ "agent_cli": "claude" });
        let id = db.create_workflow(&s, &options).unwrap();
        let w = db.workflow(&id).unwrap();
        assert_eq!(w.id, id);
        assert_eq!(w.spec, s);
        assert_eq!(w.spec_digest, s.digest());
        assert_eq!(w.options, options);
        assert!(!w.approved && !w.cancel_requested);
        assert_eq!(w.steps.len(), 3);
        for (i, st) in w.steps.iter().enumerate() {
            assert_eq!(st.idx, i);
            assert_eq!(st.name, format!("step-{i}"));
            assert!(st.task_id.is_none() && !st.skipped);
        }
        assert_eq!(task_count(&db), 0);
    }

    #[test]
    fn unknown_workflow_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let ghost = agentos_core::ids::WorkflowId::new();
        refused(db.workflow(&ghost));
        refused(db.approve_workflow(&ghost));
        refused(db.request_workflow_cancel(&ghost));
    }

    #[test]
    fn approval_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let id = db.create_workflow(&spec(2), &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let first = {
            let tx = db.immediate().unwrap();
            tx.query_row(
                "SELECT approved_ts FROM workflows WHERE id = ?1",
                [id.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        std::thread::sleep(std::time::Duration::from_millis(1100));
        db.approve_workflow(&id).unwrap();
        let second = {
            let tx = db.immediate().unwrap();
            tx.query_row(
                "SELECT approved_ts FROM workflows WHERE id = ?1",
                [id.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(first, second);
        assert!(db.workflow(&id).unwrap().approved);
    }

    #[test]
    fn step_task_needs_approval() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(2);
        let id = db.create_workflow(&s, &json!({})).unwrap();
        let c = &s.steps[0].contract;
        refused(db.create_step_task(&id, 0, c, &s.digest()));
        assert_eq!(task_count(&db), 0);
    }

    #[test]
    fn step_one_waits_for_step_zero_to_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(2);
        let id = db.create_workflow(&s, &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let d = s.digest();
        refused(db.create_step_task(&id, 1, &s.steps[1].contract, &d));
        let t0 = db
            .create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        // The previous task exists but has not succeeded.
        refused(db.create_step_task(&id, 1, &s.steps[1].contract, &d));
        set_state(&db, &t0, TaskState::Succeeded);
        let t1 = db
            .create_step_task(&id, 1, &s.steps[1].contract, &d)
            .unwrap();
        let w = db.workflow(&id).unwrap();
        assert_eq!(w.steps[0].task_id.as_ref(), Some(&t0));
        assert_eq!(w.steps[1].task_id.as_ref(), Some(&t1));
        assert_eq!(task_count(&db), 2);
        // An index outside the chain is refused too.
        refused(db.create_step_task(&id, 2, &s.steps[1].contract, &d));
    }

    #[test]
    fn step_zero_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(2);
        let id = db.create_workflow(&s, &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let d = s.digest();
        db.create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        refused(db.create_step_task(&id, 0, &s.steps[0].contract, &d));
        assert_eq!(task_count(&db), 1);
    }

    #[test]
    fn failed_step_blocks_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(2);
        let id = db.create_workflow(&s, &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let d = s.digest();
        let t0 = db
            .create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        db.append(&t0, &TaskEvent::Started).unwrap();
        db.append(&t0, &TaskEvent::Failed { reason: "x".into() })
            .unwrap();
        refused(db.create_step_task(&id, 1, &s.steps[1].contract, &d));
        assert_eq!(task_count(&db), 1);
    }

    #[test]
    fn cancel_skips_only_steps_without_a_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(3);
        let id = db.create_workflow(&s, &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let d = s.digest();
        let t0 = db
            .create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        db.request_workflow_cancel(&id).unwrap();
        let w = db.workflow(&id).unwrap();
        assert!(w.cancel_requested);
        assert!(!w.steps[0].skipped && w.steps[0].task_id.as_ref() == Some(&t0));
        assert!(w.steps[1].skipped && w.steps[2].skipped);
        set_state(&db, &t0, TaskState::Succeeded);
        refused(db.create_step_task(&id, 1, &s.steps[1].contract, &d));
        assert_eq!(task_count(&db), 1);
        // Cancelling again changes nothing.
        db.request_workflow_cancel(&id).unwrap();
    }

    #[test]
    fn fail_skips_the_steps_without_a_task_and_keeps_the_first_reason() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(3);
        let id = db
            .create_workflow(&s, &json!({ "worker": "host" }))
            .unwrap();
        db.approve_workflow(&id).unwrap();
        let d = s.digest();
        let t0 = db
            .create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        set_state(&db, &t0, TaskState::Succeeded);

        db.fail_workflow(&id, "handoff result mismatch").unwrap();

        let w = db.workflow(&id).unwrap();
        assert_eq!(
            w.options,
            json!({ "worker": "host", "failure": "handoff result mismatch" })
        );
        assert!(!w.cancel_requested);
        assert!(!w.steps[0].skipped && w.steps[0].task_id.as_ref() == Some(&t0));
        assert!(w.steps[1].skipped && w.steps[2].skipped);
        refused(db.create_step_task(&id, 1, &s.steps[1].contract, &d));
        assert_eq!(task_count(&db), 1);
        db.fail_workflow(&id, "another reason").unwrap();
        assert_eq!(
            db.workflow(&id).unwrap().options["failure"],
            "handoff result mismatch"
        );
        refused(db.fail_workflow(&agentos_core::ids::WorkflowId::new(), "x"));
    }

    #[test]
    fn step_task_matches_create_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let s = spec(2);
        let d = s.digest();
        let plain = db.create_task(&s.steps[0].contract, &d).unwrap();
        let id = db.create_workflow(&s, &json!({})).unwrap();
        db.approve_workflow(&id).unwrap();
        let step = db
            .create_step_task(&id, 0, &s.steps[0].contract, &d)
            .unwrap();
        let (a, b) = (db.task(&plain).unwrap(), db.task(&step).unwrap());
        assert_eq!(a.state, b.state);
        assert_eq!(a.workspace_digest, b.workspace_digest);
        assert_eq!(db.events(&step).unwrap()[0].event_type, "TaskCreated");
    }

    /// The version-2 schema as it was before the workflow tables.
    const V2_SCHEMA: &str = "
CREATE TABLE tasks(
    id TEXT PRIMARY KEY, owner TEXT NOT NULL, contract_digest TEXT NOT NULL,
    contract_json TEXT NOT NULL, state TEXT NOT NULL, cancel_requested INTEGER NOT NULL,
    workspace_digest TEXT NOT NULL, verified_digest TEXT, actions_used INTEGER NOT NULL,
    step INTEGER NOT NULL, checkpoint TEXT, deadline_ts INTEGER NOT NULL, created_ts INTEGER NOT NULL
);
CREATE TABLE events(
    task_id TEXT NOT NULL REFERENCES tasks(id), seq INTEGER NOT NULL, type TEXT NOT NULL,
    payload TEXT NOT NULL, ts INTEGER NOT NULL, refs TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY(task_id, seq)
);
CREATE TABLE effects(
    effect_id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), step INTEGER NOT NULL,
    kind TEXT NOT NULL, state TEXT NOT NULL, request_digest TEXT NOT NULL, expected_workspace TEXT,
    lease_generation INTEGER NOT NULL DEFAULT 0, result_digest TEXT,
    created_ts INTEGER NOT NULL, updated_ts INTEGER NOT NULL
);
CREATE TABLE attempts(
    attempt_id TEXT PRIMARY KEY, effect_id TEXT NOT NULL REFERENCES effects(effect_id),
    worker TEXT NOT NULL, lease_generation INTEGER NOT NULL, started_ts INTEGER NOT NULL,
    finished_ts INTEGER
);
CREATE TABLE artifacts(
    digest TEXT PRIMARY KEY, size INTEGER NOT NULL, type TEXT NOT NULL, provenance TEXT,
    created_ts INTEGER NOT NULL
);
CREATE TABLE artifact_links(
    digest TEXT NOT NULL REFERENCES artifacts(digest),
    effect_id TEXT NOT NULL REFERENCES effects(effect_id), PRIMARY KEY(digest, effect_id)
);
CREATE TABLE usage(
    id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL REFERENCES tasks(id),
    effect_id TEXT UNIQUE REFERENCES effects(effect_id), kind TEXT NOT NULL,
    reserved_model_requests INTEGER NOT NULL, reserved_tool_actions INTEGER NOT NULL,
    settled_model_requests INTEGER, settled_tool_actions INTEGER,
    status TEXT NOT NULL CHECK(status IN ('Reserved', 'Settled', 'Uncertain', 'Released'))
);
CREATE TABLE capabilities(
    id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), operation TEXT NOT NULL,
    scope TEXT NOT NULL, created_ts INTEGER NOT NULL, expires_ts INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0, UNIQUE(task_id, operation)
);
CREATE TABLE observations(
    id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL REFERENCES tasks(id),
    source TEXT NOT NULL, observed_revision TEXT NOT NULL, observed_ts INTEGER NOT NULL,
    evidence_ref TEXT
);
";

    #[test]
    fn a_version_2_database_is_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentos.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            let s = spec(2);
            conn.execute(
                "INSERT INTO tasks VALUES('old-task', 'local-owner', ?1, ?2, 'Created', 0, ?1, NULL, 0, 0, NULL, 0, 1)",
                params![s.digest().to_string(), serde_json::to_string(&s.steps[0].contract).unwrap()],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(SCHEMA_VERSION, 3);
        assert_eq!(db.pragma_string("user_version").unwrap(), "3");
        assert_eq!(task_count(&db), 1);
        let id = db.create_workflow(&spec(2), &json!({})).unwrap();
        assert_eq!(db.workflow(&id).unwrap().steps.len(), 2);
        drop(db);
        // Reopening at the current version is a no-op.
        drop(Db::open(&path).unwrap());
    }
}
