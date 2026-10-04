//! Parameterized, byte-bounded snapshot views for local clients.
use crate::db::{Db, DbError, Result, StoredEvent, digest_from_str, state_from_str, state_to_str};
use crate::effects::{EFFECT_COLUMNS, effect_from_row, effect_row};
use agentos_core::contract::Contract;
use agentos_core::effect::{EffectId, EffectRecord};
use agentos_core::ids::TaskId;
use agentos_core::state::{Task, TaskState};
use rusqlite::{OptionalExtension, Transaction, params};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskCursor {
    pub created_ts: i64,
    pub id: TaskId,
    pub filter: Option<TaskState>,
}
#[derive(Debug, serde::Serialize)]
pub struct TaskListRow {
    pub task: Task,
    pub goal: String,
    pub repository_source: String,
    pub created_ts: i64,
    pub text_truncated: bool,
}
#[derive(Debug, serde::Serialize)]
pub struct TaskPage {
    pub rows: Vec<TaskListRow>,
    pub next: Option<TaskCursor>,
}
#[derive(Debug, serde::Serialize)]
pub struct EventHeader {
    pub seq: u64,
    pub event_type: String,
    pub ts: i64,
    pub truncated: bool,
}
#[derive(Debug, serde::Serialize)]
pub struct EventPage {
    pub rows: Vec<EventHeader>,
    pub last_seq: u64,
    pub has_more: bool,
}
fn ensure_task(tx: &Transaction<'_>, task: &TaskId) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
        [task.as_str()],
        |r| r.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(DbError::NotFound(task.clone()))
    }
}
fn check_size(size: i64, limit: u64) -> Result<()> {
    let size =
        u64::try_from(size).map_err(|_| DbError::Corrupt("negative stored byte length".into()))?;
    if size > limit {
        Err(DbError::ReadLimit { limit })
    } else {
        Ok(())
    }
}
fn prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(e) => String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(),
    }
}
fn event_row(task: &TaskId, r: &rusqlite::Row<'_>) -> Result<StoredEvent> {
    let seq: i64 = r.get(0)?;
    Ok(StoredEvent {
        task_id: task.clone(),
        seq: u64::try_from(seq)
            .map_err(|_| DbError::Corrupt("negative journal sequence".into()))?,
        event_type: r.get(1)?,
        payload: serde_json::from_str(&r.get::<_, String>(2)?)?,
        ts: r.get(3)?,
    })
}
impl Db {
    pub fn tasks_page(
        &self,
        filter: Option<TaskState>,
        before: Option<&TaskCursor>,
        limit: usize,
    ) -> Result<TaskPage> {
        if !(1..=50).contains(&limit)
            || before.is_some_and(|c| {
                c.filter != filter
                    || c.created_ts < 0
                    || uuid::Uuid::parse_str(c.id.as_str()).is_err()
            })
        {
            return Err(DbError::InvalidQuery(
                "invalid task page limit or cursor".into(),
            ));
        }
        let tx = self.read()?;
        // CASE is lazy: oversized legacy JSON is never handed to json_extract.
        // BLOB substr bounds transferred bytes even for multibyte labels.
        let mut stmt = tx.prepare("SELECT id,state,cancel_requested,workspace_digest,verified_digest,actions_used,step,created_ts,
            CASE WHEN length(CAST(contract_json AS BLOB))>262144 THEN CAST('Goal unavailable: contract exceeds UI limit' AS BLOB)
                 ELSE substr(CAST(json_extract(contract_json,'$.goal') AS BLOB),1,4096) END,
            CASE WHEN length(CAST(contract_json AS BLOB))>262144 THEN CAST('' AS BLOB)
                 ELSE substr(CAST(json_extract(contract_json,'$.repository.source') AS BLOB),1,4096) END,
            CASE WHEN length(CAST(contract_json AS BLOB))>262144 THEN 1
                 ELSE length(CAST(json_extract(contract_json,'$.goal') AS BLOB))>4096
                      OR length(CAST(json_extract(contract_json,'$.repository.source') AS BLOB))>4096 END
            FROM tasks WHERE (?1 IS NULL OR state=?1)
            AND (?2 IS NULL OR created_ts<?2 OR (created_ts=?2 AND id<?3))
            ORDER BY created_ts DESC,id DESC LIMIT ?4")?;
        let state = filter.map(state_to_str).transpose()?;
        let mut rows = stmt.query(params![
            state,
            before.map(|c| c.created_ts),
            before.map(|c| c.id.as_str()),
            (limit + 1) as i64
        ])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let id = serde_json::from_value(serde_json::Value::String(r.get(0)?))?;
            out.push(TaskListRow {
                task: Task {
                    id,
                    state: state_from_str(r.get(1)?)?,
                    cancel_requested: r.get(2)?,
                    workspace_digest: digest_from_str(&r.get::<_, String>(3)?)?,
                    verified_digest: r
                        .get::<_, Option<String>>(4)?
                        .as_deref()
                        .map(digest_from_str)
                        .transpose()?,
                    actions_used: r.get(5)?,
                    step: r.get(6)?,
                },
                created_ts: r.get(7)?,
                goal: prefix(&r.get::<_, Vec<u8>>(8)?),
                repository_source: prefix(&r.get::<_, Vec<u8>>(9)?),
                text_truncated: r.get(10)?,
            });
        }
        let next = if out.len() > limit {
            out.pop();
            out.last().map(|r| TaskCursor {
                created_ts: r.created_ts,
                id: r.task.id.clone(),
                filter,
            })
        } else {
            None
        };
        Ok(TaskPage { rows: out, next })
    }
    pub fn event_headers(&self, task: &TaskId, after: u64, limit: usize) -> Result<EventPage> {
        if !(1..=100).contains(&limit) || after > i64::MAX as u64 {
            return Err(DbError::InvalidQuery(
                "invalid event limit or sequence".into(),
            ));
        }
        let tx = self.read()?;
        ensure_task(&tx, task)?;
        let mut stmt = tx.prepare("SELECT seq,substr(type,1,256),ts,length(CAST(payload AS BLOB))>65536 OR length(CAST(type AS BLOB))>256
            FROM events WHERE task_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?;
        let mut rows = stmt.query(params![task.as_str(), after as i64, (limit + 1) as i64])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(EventHeader {
                seq: r
                    .get::<_, i64>(0)?
                    .try_into()
                    .map_err(|_| DbError::Corrupt("negative journal sequence".into()))?,
                event_type: r.get(1)?,
                ts: r.get(2)?,
                truncated: r.get(3)?,
            });
        }
        let has_more = out.len() > limit;
        if has_more {
            out.pop();
        }
        let last_seq = out.last().map_or(after, |r| r.seq);
        Ok(EventPage {
            rows: out,
            last_seq,
            has_more,
        })
    }
    pub fn events_bounded(&self, task: &TaskId, max_bytes: u64) -> Result<Vec<StoredEvent>> {
        let tx = self.read()?;
        ensure_task(&tx, task)?;
        let size = tx.query_row("SELECT coalesce(sum(length(CAST(payload AS BLOB))+length(CAST(type AS BLOB))),0) FROM events WHERE task_id=?1",[task.as_str()],|r|r.get(0))?;
        check_size(size, max_bytes)?;
        let mut stmt =
            tx.prepare("SELECT seq,type,payload,ts FROM events WHERE task_id=?1 ORDER BY seq")?;
        let mut rows = stmt.query([task.as_str()])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(event_row(task, r)?);
        }
        Ok(out)
    }
    pub fn first_event_bounded(
        &self,
        task: &TaskId,
        kind: &str,
        max_bytes: Option<u64>,
    ) -> Result<Option<StoredEvent>> {
        let tx = self.read()?;
        ensure_task(&tx, task)?;
        let found: Option<(i64,i64)> = tx.query_row("SELECT seq,length(CAST(payload AS BLOB))+length(CAST(type AS BLOB)) FROM events WHERE task_id=?1 AND type=?2 ORDER BY seq LIMIT 1",params![task.as_str(),kind],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((seq, size)) = found else {
            return Ok(None);
        };
        if let Some(limit) = max_bytes {
            check_size(size, limit)?;
        }
        let mut stmt =
            tx.prepare("SELECT seq,type,payload,ts FROM events WHERE task_id=?1 AND seq=?2")?;
        let mut rows = stmt.query(params![task.as_str(), seq])?;
        rows.next()?.map(|r| event_row(task, r)).transpose()
    }
    pub fn contract_bounded(&self, task: &TaskId, max_bytes: u64) -> Result<Contract> {
        let tx = self.read()?;
        let size = tx
            .query_row(
                "SELECT length(CAST(contract_json AS BLOB)) FROM tasks WHERE id=?1",
                [task.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| DbError::NotFound(task.clone()))?;
        check_size(size, max_bytes)?;
        let json: String = tx.query_row(
            "SELECT contract_json FROM tasks WHERE id=?1",
            [task.as_str()],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&json)?)
    }
    pub fn effect_bounded(&self, effect: &EffectId, max_bytes: u64) -> Result<EffectRecord> {
        let tx = self.read()?;
        let size = tx
            .query_row(
                "SELECT length(CAST(kind AS BLOB)) FROM effects WHERE effect_id=?1",
                [effect.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| DbError::EffectNotFound(effect.clone()))?;
        check_size(size, max_bytes)?;
        let row = tx.query_row(
            &format!("SELECT {EFFECT_COLUMNS} FROM effects WHERE effect_id=?1"),
            [effect.as_str()],
            effect_row,
        )?;
        effect_from_row(row)
    }
    pub fn outstanding_effects_bounded(
        &self,
        task: &TaskId,
        max_bytes: u64,
    ) -> Result<Vec<EffectRecord>> {
        let tx = self.read()?;
        ensure_task(&tx, task)?;
        let size = tx.query_row("SELECT coalesce(sum(length(CAST(kind AS BLOB))),0) FROM effects WHERE task_id=?1 AND state IN ('INTENDED','DISPATCHED','UNKNOWN')",[task.as_str()],|r|r.get(0))?;
        check_size(size, max_bytes)?;
        let mut stmt = tx.prepare(&format!("SELECT {EFFECT_COLUMNS} FROM effects WHERE task_id=?1 AND state IN ('INTENDED','DISPATCHED','UNKNOWN') ORDER BY rowid"))?;
        let rows = stmt.query_map([task.as_str()], effect_row)?;
        rows.map(|r| effect_from_row(r?)).collect()
    }
}
