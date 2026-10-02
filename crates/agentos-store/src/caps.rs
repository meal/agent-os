//! The capability broker's persistence: handles issued at approval, the journaled
//! authorization taken inside `record_intent` and `mark_dispatched`, a pure `check` for
//! pre-checks, and revocation.
//!
//! A full handle lives only in `capabilities.id`. Journal payloads, errors and logs carry
//! its 8-character prefix at most.

use agentos_core::broker::{authorize, scope_for, CapabilityGrant, Handle, Resource, Scope};
use agentos_core::contract::Capability;
use agentos_core::ids::TaskId;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

use crate::db::{insert_event, load_task, now_ts, Db, DbError, Result};

const NOT_APPROVED: &str = "not_approved";
const UNKNOWN_HANDLE: &str = "unknown_handle";

fn op_str(op: Capability) -> Result<String> {
    match serde_json::to_value(op)? {
        Value::String(s) => Ok(s),
        other => Err(DbError::Corrupt(format!("capability serialized as {other}"))),
    }
}

/// What a decision was about, short enough for the journal: `task`, `paths:<n>`, `profile:<id>`.
fn describe(resource: &Resource) -> String {
    match resource {
        Resource::Task => "task".into(),
        Resource::Paths(paths) => format!("paths:{}", paths.len()),
        Resource::Profile(id) => format!("profile:{id}"),
    }
}

/// The resource a grant's whole scope covers: a dispatch re-checks revocation and expiry of
/// the handle the intent was authorized under, not the request again.
fn scope_resource(scope: &Scope) -> Resource {
    match scope {
        Scope::Task => Resource::Task,
        Scope::Paths(p) => Resource::Paths(p.clone()),
        Scope::Profile(id) => Resource::Profile(id.clone()),
    }
}

pub(crate) fn deadline_of(tx: &Transaction, task: &TaskId) -> Result<i64> {
    tx.query_row("SELECT deadline_ts FROM tasks WHERE id = ?1", [task.as_str()], |r| r.get(0))
        .optional()?
        .ok_or_else(|| DbError::NotFound(task.clone()))
}

fn load_grants(tx: &Transaction, task: &TaskId) -> Result<Vec<CapabilityGrant>> {
    let mut stmt = tx.prepare(
        "SELECT id, operation, scope, expires_ts, revoked FROM capabilities WHERE task_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map([task.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, operation, scope, expires_ts, revoked) = row?;
        out.push(CapabilityGrant {
            // The error names no part of the stored value.
            handle: Handle::parse(&id).map_err(|_| DbError::Corrupt("malformed capability handle".into()))?,
            task: task.clone(),
            operation: serde_json::from_value(Value::String(operation))?,
            scope: serde_json::from_str(&scope)?,
            expires_ts,
            revoked,
        });
    }
    Ok(out)
}

/// A refused decision: the reason and the prefix of the handle involved, if any.
struct Refusal {
    reason: &'static str,
    prefix: Option<String>,
}

/// The broker decision for `op` on `resource`, without side effects.
fn decide(tx: &Transaction, task: &TaskId, op: Capability, resource: &Resource, now: i64) -> Result<std::result::Result<Handle, Refusal>> {
    if deadline_of(tx, task)? == 0 {
        return Ok(Err(Refusal { reason: NOT_APPROVED, prefix: None }));
    }
    let Some(grant) = load_grants(tx, task)?.into_iter().find(|g| g.operation == op) else {
        return Ok(Err(Refusal { reason: UNKNOWN_HANDLE, prefix: None }));
    };
    Ok(match authorize(&grant, task, op, resource, now) {
        Ok(()) => Ok(grant.handle),
        Err(d) => Err(Refusal { reason: d.reason(), prefix: Some(grant.handle.prefix().to_string()) }),
    })
}

/// Journaled authorization inside the caller's write transaction. A grant journals
/// `CapabilityGranted` in `tx` (it commits or rolls back with the caller's work). A denial
/// journals `CapabilityDenied` in `tx` and returns `DbError::CapabilityDenied`: the caller
/// must then COMMIT `tx` (having written nothing else in it yet) so the denial is durable
/// although the operation is refused.
pub(crate) fn authorize_in(tx: &Transaction, task: &TaskId, op: Capability, resource: &Resource, now: i64) -> Result<Handle> {
    let mut payload = json!({ "operation": op_str(op)?, "resource": describe(resource) });
    match decide(tx, task, op, resource, now)? {
        Ok(handle) => {
            payload["handle_prefix"] = json!(handle.prefix());
            insert_event(tx, task, "CapabilityGranted", &payload)?;
            Ok(handle)
        }
        Err(Refusal { reason, prefix }) => {
            payload["reason"] = json!(reason);
            if let Some(prefix) = prefix {
                payload["handle_prefix"] = json!(prefix);
            }
            insert_event(tx, task, "CapabilityDenied", &payload)?;
            Err(DbError::CapabilityDenied { capability: op, reason: reason.to_string() })
        }
    }
}

/// [`authorize_in`] for a dispatch: re-checks the handle for `op` (revoked, expired) over
/// its whole scope. Same commit rule on denial.
pub(crate) fn reauthorize_in(tx: &Transaction, task: &TaskId, op: Capability, now: i64) -> Result<Handle> {
    let resource = load_grants(tx, task)?
        .into_iter()
        .find(|g| g.operation == op)
        .map_or(Resource::Task, |g| scope_resource(&g.scope));
    authorize_in(tx, task, op, &resource, now)
}

fn handle_list(grants: &[CapabilityGrant]) -> Result<Value> {
    let operations = grants.iter().map(|g| op_str(g.operation)).collect::<Result<Vec<_>>>()?;
    let handles = grants
        .iter()
        .zip(&operations)
        .map(|(g, op)| json!({ "operation": op, "prefix": g.handle.prefix() }))
        .collect::<Vec<_>>();
    Ok(json!({ "operations": operations, "handles": handles }))
}

impl Db {
    /// The owner approved `task`: starts its deadline (`now + deadline_seconds`) and issues one
    /// handle per contract capability, scoped by the contract, expiring at the deadline
    /// (`artifact.export` never expires). Journals one `CapabilitiesIssued` with prefixes.
    /// Idempotent: a later call returns the issued operations and writes nothing.
    pub fn approve_task(&self, task: &TaskId) -> Result<Vec<Capability>> {
        let tx = self.immediate()?;
        let (_, contract) = load_task(&tx, task)?;
        if deadline_of(&tx, task)? != 0 {
            return Ok(load_grants(&tx, task)?.into_iter().map(|g| g.operation).collect());
        }
        let deadline = self.now().saturating_add(i64::from(contract.limits.deadline_seconds));
        tx.execute("UPDATE tasks SET deadline_ts = ?2 WHERE id = ?1", params![task.as_str(), deadline])?;
        let created = now_ts();
        let mut ops: Vec<Capability> = Vec::new();
        for op in &contract.capabilities {
            if ops.contains(op) {
                continue;
            }
            ops.push(*op);
            let expires = (*op != Capability::ArtifactExport).then_some(deadline);
            tx.execute(
                "INSERT INTO capabilities(id, task_id, operation, scope, created_ts, expires_ts, revoked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    Handle::generate().to_string(),
                    task.as_str(),
                    op_str(*op)?,
                    serde_json::to_string(&scope_for(*op, &contract))?,
                    created,
                    expires
                ],
            )?;
        }
        insert_event(&tx, task, "CapabilitiesIssued", &handle_list(&load_grants(&tx, task)?)?)?;
        tx.commit()?;
        Ok(ops)
    }

    /// Pure broker check for pre-checks: journals nothing. Denial reasons are those of
    /// `Denial::reason()`, plus `not_approved` (no approval yet) and `unknown_handle` (no
    /// handle was issued for `op`).
    pub fn check(&self, task: &TaskId, op: Capability, resource: &Resource) -> Result<()> {
        let tx = self.read()?;
        load_task(&tx, task)?;
        match decide(&tx, task, op, resource, self.now())? {
            Ok(_) => Ok(()),
            Err(r) => Err(DbError::CapabilityDenied { capability: op, reason: r.reason.to_string() }),
        }
    }

    /// Revokes the task's live handles (only `only`'s, when given). Returns the operations
    /// actually revoked; journals `CapabilityRevoked` only when something changed.
    pub fn revoke(&self, task: &TaskId, only: Option<Capability>) -> Result<Vec<Capability>> {
        let tx = self.immediate()?;
        load_task(&tx, task)?;
        let live: Vec<CapabilityGrant> = load_grants(&tx, task)?
            .into_iter()
            .filter(|g| !g.revoked && only.is_none_or(|op| g.operation == op))
            .collect();
        if live.is_empty() {
            return Ok(Vec::new());
        }
        for g in &live {
            tx.execute(
                "UPDATE capabilities SET revoked = 1 WHERE task_id = ?1 AND operation = ?2",
                params![task.as_str(), op_str(g.operation)?],
            )?;
        }
        insert_event(&tx, task, "CapabilityRevoked", &handle_list(&live)?)?;
        tx.commit()?;
        Ok(live.into_iter().map(|g| g.operation).collect())
    }

    /// The task's handles, in issue order.
    pub fn grants(&self, task: &TaskId) -> Result<Vec<CapabilityGrant>> {
        let tx = self.read()?;
        load_task(&tx, task)?;
        load_grants(&tx, task)
    }

    /// Unix seconds at which the task's deadline passes; 0 until it is approved.
    pub fn deadline_ts(&self, task: &TaskId) -> Result<i64> {
        let tx = self.read()?;
        deadline_of(&tx, task)
    }

    /// True once an approved task's deadline has passed (by the injectable clock).
    pub fn deadline_passed(&self, task: &TaskId) -> Result<bool> {
        let deadline = self.deadline_ts(task)?;
        Ok(deadline != 0 && self.now() >= deadline)
    }
}
