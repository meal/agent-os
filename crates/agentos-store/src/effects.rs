//! Effect lifecycle: intent, dispatch, receipt, unknown; with budget reservations.
//!
//! Every write runs in one `BEGIN IMMEDIATE` transaction and appends an event row in that
//! same transaction, so the journal never disagrees with the effect, usage and task rows.
//!
//! Publish ordering (engine contract): write result bytes with `BlobStore::put` FIRST, then
//! `register_artifact(digest, size, type, Some(effect), provenance)`, then `complete_effect`.
//! Blobs are content-addressed, so several effects may produce the same bytes: `artifacts`
//! holds one content row per digest and `artifact_links` records which effects produced it.
//! A successful completion must name its artifact, and the artifact must be linked to that
//! same effect; `effects.result_digest` is set only from that artifact (a failure may
//! name an error artifact under the same rules, or none). So a committed effect never
//! references a blob that was not published. New workspace digests travel in the follow-up
//! `TaskEvent`, not in `result_digest`.
//!
//! Effect ids are derived from the task's step at intent time. Recording an intent for
//! `ReadSnapshot`/`ApplyPatch` also applies `ActionUsed`, which advances the step, so a retry
//! of that same intent finds it at `step - 1`. The idempotency window is "no other accepted
//! `TaskEvent`": every accepted event advances the step, while dispatch, completion, artifact
//! and audit rows do not. So a repeated intent after the effect completed still returns the
//! existing (now finished) record, and only after another task event is the same request a
//! new effect.
//!
//! Usage: one row per effect. Status `Reserved` -> `Settled` on a receipt, or `Reserved` ->
//! `Uncertain` when the effect becomes unknown; an uncertain reservation is never released
//! because the effect may have run. It stays `Uncertain` across a re-dispatch and settles
//! when a receipt finally arrives.

use std::collections::HashSet;

use agentos_core::budget::{check_model_budget, tool_actions_for, BudgetError, Reservation, UsageTotals};
use agentos_core::effect::{
    accept_receipt, AttemptId, EffectId, EffectKind, EffectRecord, EffectState, Outcome, Receipt,
    ReceiptVerdict,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{reduce, TaskEvent, TransitionError};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::json;

use crate::db::{
    digest_from_str, event_name, insert_event, load_task, now_ts, store_task, Db, DbError, Result,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageSummary {
    pub reserved_model_requests: u64,
    pub settled_model_requests: u64,
    pub uncertain_model_requests: u64,
    pub reserved_tool_actions: u64,
    pub settled_tool_actions: u64,
    pub uncertain_tool_actions: u64,
}

impl UsageSummary {
    pub fn model_totals(&self) -> UsageTotals {
        UsageTotals {
            reserved_model_requests: self.reserved_model_requests,
            settled_model_requests: self.settled_model_requests,
            uncertain_model_requests: self.uncertain_model_requests,
        }
    }
}

const EFFECT_COLUMNS: &str =
    "effect_id, task_id, step, kind, state, request_digest, lease_generation, result_digest";

fn effect_state_str(s: EffectState) -> &'static str {
    match s {
        EffectState::Intended => "INTENDED",
        EffectState::Dispatched => "DISPATCHED",
        EffectState::Completed => "COMPLETED",
        EffectState::Failed => "FAILED",
        EffectState::Unknown => "UNKNOWN",
    }
}

fn effect_state_from(s: &str) -> Result<EffectState> {
    Ok(match s {
        "INTENDED" => EffectState::Intended,
        "DISPATCHED" => EffectState::Dispatched,
        "COMPLETED" => EffectState::Completed,
        "FAILED" => EffectState::Failed,
        "UNKNOWN" => EffectState::Unknown,
        other => return Err(DbError::Corrupt(format!("effect state {other:?}"))),
    })
}

type EffectRow = (String, String, u32, String, String, String, i64, Option<String>);

/// SQLite integers are i64; counters and generations are u64 in the API.
fn from_sql_int(v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| DbError::Corrupt(format!("negative counter {v}")))
}

fn to_sql_int(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| DbError::Corrupt(format!("value {v} exceeds i64")))
}

fn effect_row(r: &rusqlite::Row) -> rusqlite::Result<EffectRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

fn effect_from_row(row: EffectRow) -> Result<EffectRecord> {
    let (effect_id, task_id, step, kind, state, request_digest, lease_generation, result_digest) = row;
    Ok(EffectRecord {
        effect_id: serde_json::from_value(serde_json::Value::String(effect_id))?,
        task_id: serde_json::from_value(serde_json::Value::String(task_id))?,
        step,
        kind: serde_json::from_str(&kind)?,
        state: effect_state_from(&state)?,
        request_digest: digest_from_str(&request_digest)?,
        lease_generation: from_sql_int(lease_generation)?,
        result_digest: result_digest.as_deref().map(digest_from_str).transpose()?,
    })
}

fn find_effect(tx: &Transaction, id: &EffectId) -> Result<Option<EffectRecord>> {
    tx.query_row(
        &format!("SELECT {EFFECT_COLUMNS} FROM effects WHERE effect_id = ?1"),
        [id.as_str()],
        effect_row,
    )
    .optional()?
    .map(effect_from_row)
    .transpose()
}

fn load_effect(tx: &Transaction, id: &EffectId) -> Result<EffectRecord> {
    find_effect(tx, id)?.ok_or_else(|| DbError::EffectNotFound(id.clone()))
}

/// Buckets each usage row once, by status: a settled row counts only its settled amount.
fn usage_totals(tx: &Transaction, task: &TaskId) -> Result<UsageSummary> {
    let mut stmt = tx.prepare(
        "SELECT status,
                COALESCE(SUM(CASE WHEN status = 'Settled' THEN settled_model_requests ELSE reserved_model_requests END), 0),
                COALESCE(SUM(CASE WHEN status = 'Settled' THEN settled_tool_actions ELSE reserved_tool_actions END), 0)
         FROM usage WHERE task_id = ?1 GROUP BY status",
    )?;
    let rows = stmt.query_map([task.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut s = UsageSummary::default();
    for row in rows {
        let (status, model, tools) = row?;
        let (model, tools) = (from_sql_int(model)?, from_sql_int(tools)?);
        match status.as_str() {
            "Reserved" => (s.reserved_model_requests, s.reserved_tool_actions) = (model, tools),
            "Settled" => (s.settled_model_requests, s.settled_tool_actions) = (model, tools),
            "Uncertain" => (s.uncertain_model_requests, s.uncertain_tool_actions) = (model, tools),
            other => return Err(DbError::Corrupt(format!("usage status {other:?}"))),
        }
    }
    Ok(s)
}

fn expect_one(n: usize, what: &str, effect: &EffectId) -> Result<()> {
    if n == 1 {
        Ok(())
    } else {
        Err(DbError::Corrupt(format!("{what} for effect {effect} touched {n} rows")))
    }
}

impl Db {
    /// Records an effect as INTENDED together with its usage reservation, any tool action it
    /// consumes, and an `EffectIntended` event. Idempotent: an existing effect is returned as is.
    pub fn record_intent(
        &self,
        task_id: &TaskId,
        kind: EffectKind,
        request_digest: Digest,
        expected_workspace: &Digest,
        reserve: Reservation,
    ) -> Result<EffectRecord> {
        let tx = self.immediate()?;
        let (task, contract) = load_task(&tx, task_id)?;
        let effect_id = EffectId::derive(task_id, task.step, &kind, &request_digest);
        if let Some(existing) = find_effect(&tx, &effect_id)? {
            return Ok(existing);
        }
        let consumes = tool_actions_for(&kind);
        if consumes > 0 && task.step > 0 {
            let prev = EffectId::derive(task_id, task.step - 1, &kind, &request_digest);
            if let Some(existing) = find_effect(&tx, &prev)? {
                return Ok(existing);
            }
        }

        if !task.may_dispatch() {
            return Err(DbError::NotDispatchable {
                state: task.state,
                cancel_requested: task.cancel_requested,
            });
        }
        if reserve.tool_actions != consumes {
            return Err(DbError::InvalidReservation { expected: consumes, got: reserve.tool_actions });
        }
        let capability = kind.capability();
        if !contract.capabilities.contains(&capability) {
            drop(tx);
            self.append_audit(
                task_id,
                "Denied",
                &json!({
                    "reason": "CapabilityDenied",
                    "capability": capability,
                    "effect_id": effect_id,
                    "kind": kind,
                }),
            )?;
            return Err(DbError::CapabilityDenied(capability));
        }
        if let EffectKind::ApplyPatch { expected_base } = &kind {
            let actual = task.workspace_digest;
            if let Some(expected) = [*expected_workspace, *expected_base].into_iter().find(|d| *d != actual) {
                drop(tx);
                self.append_audit(
                    task_id,
                    "Denied",
                    &json!({
                        "reason": "VersionConflict",
                        "expected": expected,
                        "actual": actual,
                        "effect_id": effect_id,
                        "kind": kind,
                    }),
                )?;
                return Err(DbError::VersionConflict { expected, actual });
            }
        }
        check_model_budget(&usage_totals(&tx, task_id)?.model_totals(), &reserve, &contract.limits)
            .map_err(DbError::BudgetExceeded)?;
        let after_action = if consumes > 0 {
            match reduce(&task, &TaskEvent::ActionUsed, &contract.limits) {
                Ok(next) => Some(next),
                Err(TransitionError::ActionLimit(limit)) => {
                    return Err(DbError::BudgetExceeded(BudgetError::ToolActions { limit }))
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };

        let now = now_ts();
        tx.execute(
            "INSERT INTO effects(effect_id, task_id, step, kind, state, request_digest,
                expected_workspace, lease_generation, result_digest, created_ts, updated_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, ?8, ?8)",
            params![
                effect_id.as_str(),
                task_id.as_str(),
                task.step,
                serde_json::to_string(&kind)?,
                effect_state_str(EffectState::Intended),
                request_digest.to_string(),
                expected_workspace.to_string(),
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO usage(task_id, effect_id, kind, reserved_model_requests, reserved_tool_actions, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'Reserved')",
            params![
                task_id.as_str(),
                effect_id.as_str(),
                kind.tag(),
                reserve.model_requests,
                reserve.tool_actions
            ],
        )?;
        insert_event(
            &tx,
            task_id,
            "EffectIntended",
            &json!({
                "effect_id": effect_id,
                "kind": kind,
                "step": task.step,
                "request_digest": request_digest,
                "expected_workspace": expected_workspace,
                "reservation": reserve,
            }),
        )?;
        if let Some(next) = &after_action {
            store_task(&tx, next)?;
            insert_event(&tx, task_id, "ActionUsed", &serde_json::to_value(TaskEvent::ActionUsed)?)?;
        }
        let record = load_effect(&tx, &effect_id)?;
        tx.commit()?;
        Ok(record)
    }

    /// INTENDED -> DISPATCHED, or a re-dispatch of a DISPATCHED or UNKNOWN effect under a
    /// strictly newer lease (the superseded attempt is closed). Refused unless the task may
    /// dispatch (Running or Verifying, no cancel pending).
    pub fn mark_dispatched(
        &self,
        effect: &EffectId,
        attempt: &AttemptId,
        worker: &str,
        lease_generation: u64,
    ) -> Result<()> {
        let tx = self.immediate()?;
        let rec = load_effect(&tx, effect)?;
        let stale = match rec.state {
            EffectState::Intended => lease_generation < rec.lease_generation,
            EffectState::Dispatched | EffectState::Unknown => lease_generation <= rec.lease_generation,
            from => {
                return Err(DbError::InvalidEffectTransition {
                    effect: effect.clone(),
                    from,
                    to: EffectState::Dispatched,
                })
            }
        };
        if stale {
            return Err(DbError::StaleLease { stored: rec.lease_generation, got: lease_generation });
        }
        let (task, _) = load_task(&tx, &rec.task_id)?;
        if !task.may_dispatch() {
            return Err(DbError::NotDispatchable {
                state: task.state,
                cancel_requested: task.cancel_requested,
            });
        }

        let now = now_ts();
        tx.execute(
            "UPDATE attempts SET finished_ts = ?2 WHERE effect_id = ?1 AND finished_ts IS NULL",
            params![effect.as_str(), now],
        )?;
        tx.execute(
            "INSERT INTO attempts(attempt_id, effect_id, worker, lease_generation, started_ts, finished_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![attempt.to_string(), effect.as_str(), worker, to_sql_int(lease_generation)?, now],
        )?;
        tx.execute(
            "UPDATE effects SET state = ?2, lease_generation = ?3, updated_ts = ?4 WHERE effect_id = ?1",
            params![
                effect.as_str(),
                effect_state_str(EffectState::Dispatched),
                to_sql_int(lease_generation)?,
                now
            ],
        )?;
        insert_event(
            &tx,
            &rec.task_id,
            "EffectDispatched",
            &json!({
                "effect_id": effect,
                "attempt_id": attempt,
                "worker": worker,
                "lease_generation": lease_generation,
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Applies a worker receipt. On `Apply` the effect result, usage settlement, attempt
    /// closure, completion event and `follow_up` commit together. A follow-up the reducer
    /// rejects because cancellation is pending or the task is terminal is journaled as
    /// `TaskEventRejected` without undoing the completion; any other rejection returns
    /// `DbError::Transition` and nothing is written. Any other verdict only appends an audit
    /// event. See the module doc for the artifact rules.
    pub fn complete_effect(
        &self,
        effect: &EffectId,
        receipt: &Receipt,
        result_artifact: Option<&Digest>,
        follow_up: Option<TaskEvent>,
    ) -> Result<ReceiptVerdict> {
        let tx = self.immediate()?;
        let rec = load_effect(&tx, effect)?;
        let verdict = accept_receipt(&rec, receipt);
        if verdict != ReceiptVerdict::Apply {
            let event_type = if verdict == ReceiptVerdict::NotDispatched {
                "ReceiptRejected"
            } else {
                "ReceiptIgnored"
            };
            insert_event(
                &tx,
                &rec.task_id,
                event_type,
                &json!({
                    "reason": verdict,
                    "effect_id": effect,
                    "effect_state": rec.state,
                    "stored_lease_generation": rec.lease_generation,
                    "receipt": receipt,
                }),
            )?;
            tx.commit()?;
            return Ok(verdict);
        }
        match result_artifact {
            None if receipt.outcome == Outcome::Success => {
                return Err(DbError::ArtifactRequired(effect.clone()))
            }
            None => {}
            Some(artifact) => {
                let (published, linked): (bool, bool) = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM artifacts WHERE digest = ?1),
                            EXISTS(SELECT 1 FROM artifact_links WHERE digest = ?1 AND effect_id = ?2)",
                    [artifact.to_string(), effect.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if !published {
                    return Err(DbError::ArtifactNotPublished(*artifact));
                }
                if !linked {
                    return Err(DbError::ArtifactEffectMismatch { artifact: *artifact, effect: effect.clone() });
                }
                if let Some(d) = receipt.result_digest.filter(|d| d != artifact) {
                    return Err(DbError::ReceiptArtifactMismatch { artifact: *artifact, receipt: d });
                }
            }
        }
        let (task, contract) = load_task(&tx, &rec.task_id)?;
        let (state, event_type) = match receipt.outcome {
            Outcome::Success => (EffectState::Completed, "EffectCompleted"),
            Outcome::Failure(_) => (EffectState::Failed, "EffectFailed"),
        };

        let now = now_ts();
        tx.execute(
            "UPDATE effects SET state = ?2, result_digest = ?3, updated_ts = ?4 WHERE effect_id = ?1",
            params![
                effect.as_str(),
                effect_state_str(state),
                result_artifact.map(|d| d.to_string()),
                now
            ],
        )?;
        tx.execute(
            "UPDATE attempts SET finished_ts = ?2 WHERE effect_id = ?1 AND finished_ts IS NULL",
            params![effect.as_str(), now],
        )?;
        let settled = tx.execute(
            "UPDATE usage SET settled_model_requests = reserved_model_requests,
                settled_tool_actions = reserved_tool_actions, status = 'Settled'
             WHERE effect_id = ?1 AND status IN ('Reserved', 'Uncertain')",
            [effect.as_str()],
        )?;
        expect_one(settled, "usage settlement", effect)?;
        insert_event(
            &tx,
            &rec.task_id,
            event_type,
            &json!({
                "effect_id": effect,
                "attempt_id": receipt.attempt_id,
                "lease_generation": receipt.lease_generation,
                "outcome": receipt.outcome,
                "result_digest": receipt.result_digest,
                "result_artifact": result_artifact,
                "previous_state": rec.state,
            }),
        )?;
        if let Some(ev) = follow_up {
            match reduce(&task, &ev, &contract.limits) {
                Ok(next) => {
                    store_task(&tx, &next)?;
                    insert_event(&tx, &rec.task_id, &event_name(&ev)?, &serde_json::to_value(&ev)?)?;
                }
                Err(e @ (TransitionError::Terminal(_) | TransitionError::CancelRequested { .. })) => {
                    insert_event(
                        &tx,
                        &rec.task_id,
                        "TaskEventRejected",
                        &json!({ "event": ev, "reason": e.to_string(), "effect_id": effect }),
                    )?;
                }
                // Dropping `tx` rolls back the completion too.
                Err(e) => return Err(e.into()),
            }
        }
        tx.commit()?;
        Ok(verdict)
    }

    /// Records a published blob (content row) and, when `effect` is given, links it to that
    /// effect. Both inserts are idempotent; each new link journals `ArtifactRegistered` on the
    /// effect's task.
    pub fn register_artifact(
        &self,
        digest: &Digest,
        size: u64,
        artifact_type: &str,
        effect: Option<&EffectId>,
        provenance: &str,
    ) -> Result<()> {
        let tx = self.immediate()?;
        let owner = effect.map(|e| load_effect(&tx, e)).transpose()?;
        tx.execute(
            "INSERT OR IGNORE INTO artifacts(digest, size, type, provenance, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![digest.to_string(), to_sql_int(size)?, artifact_type, provenance, now_ts()],
        )?;
        let Some(rec) = owner else {
            tx.commit()?;
            return Ok(());
        };
        let linked = tx.execute(
            "INSERT OR IGNORE INTO artifact_links(digest, effect_id) VALUES (?1, ?2)",
            [digest.to_string(), rec.effect_id.to_string()],
        )?;
        if linked == 1 {
            insert_event(
                &tx,
                &rec.task_id,
                "ArtifactRegistered",
                &json!({
                    "digest": digest,
                    "size": size,
                    "type": artifact_type,
                    "effect_id": rec.effect_id,
                    "provenance": provenance,
                }),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// DISPATCHED -> UNKNOWN. The reservation becomes `Uncertain` and keeps counting.
    pub fn mark_unknown(&self, effect: &EffectId) -> Result<()> {
        let tx = self.immediate()?;
        let rec = load_effect(&tx, effect)?;
        if rec.state != EffectState::Dispatched {
            return Err(DbError::InvalidEffectTransition {
                effect: effect.clone(),
                from: rec.state,
                to: EffectState::Unknown,
            });
        }
        tx.execute(
            "UPDATE effects SET state = ?2, updated_ts = ?3 WHERE effect_id = ?1",
            params![effect.as_str(), effect_state_str(EffectState::Unknown), now_ts()],
        )?;
        let n = tx.execute(
            "UPDATE usage SET status = 'Uncertain' WHERE effect_id = ?1 AND status IN ('Reserved', 'Uncertain')",
            [effect.as_str()],
        )?;
        expect_one(n, "usage uncertainty", effect)?;
        insert_event(
            &tx,
            &rec.task_id,
            "EffectUnknown",
            &json!({ "effect_id": effect, "lease_generation": rec.lease_generation }),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every registered artifact digest: the `referenced` set for `BlobStore::gc`.
    pub fn referenced_blobs(&self) -> Result<HashSet<Digest>> {
        let tx = self.read()?;
        let mut stmt = tx.prepare("SELECT digest FROM artifacts")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| digest_from_str(&r?)).collect()
    }

    pub fn effect(&self, id: &EffectId) -> Result<EffectRecord> {
        let tx = self.read()?;
        load_effect(&tx, id)
    }

    /// INTENDED, DISPATCHED and UNKNOWN effects of a task, in creation order.
    pub fn outstanding_effects(&self, task: &TaskId) -> Result<Vec<EffectRecord>> {
        let tx = self.read()?;
        load_task(&tx, task)?;
        let mut stmt = tx.prepare(&format!(
            "SELECT {EFFECT_COLUMNS} FROM effects
             WHERE task_id = ?1 AND state IN ('INTENDED', 'DISPATCHED', 'UNKNOWN')
             ORDER BY rowid"
        ))?;
        let rows = stmt.query_map([task.as_str()], effect_row)?;
        rows.map(|r| effect_from_row(r?)).collect()
    }

    pub fn usage_summary(&self, task: &TaskId) -> Result<UsageSummary> {
        let tx = self.read()?;
        load_task(&tx, task)?;
        usage_totals(&tx, task)
    }
}
