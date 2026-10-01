//! Journaled agent turns and the journal lookups that make every step of a turn
//! idempotent, so a restarted controller can replay a deterministic agent and resume.
//!
//! # Turns
//! Before the runner acts on an agent decision it appends an `AgentTurn` audit event
//! `{turn, observation, action}`; the action carries the full patch text, so the patch stays
//! available even if its blob was never registered (and so was collected). A *session* is
//! the run of `AgentTurn`s since the task last (re)entered RUNNING (`Started`, `Resumed` or
//! `Woken`): a paused-then-resumed task starts a new session with a fresh `Start`
//! observation, while a crashed run's session is still open and is replayed.
//!
//! # Replay
//! The fresh agent is fed each journaled observation and must emit the journaled action;
//! nothing is executed. The last journaled action may be only partly done. Everything
//! journaled after its `AgentTurn` was caused by it (the next turn is not journaled yet),
//! so the runner re-runs it through the same code as the live path, which consults those
//! events first:
//! - an `EffectIntended` for the action's request: that effect is the action's effect.
//!   Recovery already finished it (COMPLETED/FAILED), so its observation is rebuilt from the
//!   published result blob and nothing runs again;
//! - a `Denied` event: the action was refused; its observation is rebuilt from the denial;
//! - neither: the action did nothing durable yet and is simply performed.
//!
//! Invariants this relies on:
//! - *Turn journaled, intent not recorded*: no effect and no denial follow the turn, so
//!   performing the action is its first execution.
//! - *Effect completed, next turn not journaled*: the effect is found through its intent
//!   after the turn, never intended again (a second intent could be a new effect, since the
//!   completion advanced the task step).
//! - `record_intent` is idempotent per `EffectId`; a completed effect's outcome is read
//!   from its stored result, never re-executed.

use agentos_core::contract::Capability;
use agentos_core::effect::{EffectId, EffectKind, EffectRecord, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, DbError, StoredEvent};
use serde_json::{json, Value};

use crate::agent::{AgentAction, Observation};
use crate::runner::{EngineError, Result};

pub(crate) struct Turn {
    pub seq: u64,
    pub turn: u32,
    pub observation: Observation,
    pub action: AgentAction,
}

/// Appends the turn the runner is about to act on; returns its journal sequence number.
pub(crate) fn append_turn(db: &Db, task: &TaskId, turn: u32, obs: &Observation, action: &AgentAction) -> Result<u64> {
    let payload = json!({ "turn": turn, "observation": obs, "action": action });
    Ok(db.append_audit(task, "AgentTurn", &payload)?)
}

fn decode<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T> {
    Ok(serde_json::from_value(v.clone()).map_err(DbError::from)?)
}

/// The turns of the open session, in order.
pub(crate) fn session_turns(events: &[StoredEvent]) -> Result<Vec<Turn>> {
    let start = events
        .iter()
        .rposition(|e| matches!(e.event_type.as_str(), "Started" | "Resumed" | "Woken"))
        .map_or(0, |i| i + 1);
    events[start..]
        .iter()
        .filter(|e| e.event_type == "AgentTurn")
        .map(|e| {
            Ok(Turn {
                seq: e.seq,
                turn: decode(&e.payload["turn"])?,
                observation: decode(&e.payload["observation"])?,
                action: decode(&e.payload["action"])?,
            })
        })
        .collect()
}

/// Events journaled after sequence number `since`.
pub(crate) fn events_after(db: &Db, task: &TaskId, since: u64) -> Result<Vec<StoredEvent>> {
    let mut events = db.events(task)?;
    events.retain(|e| e.seq > since);
    Ok(events)
}

/// The variant name of a serialized `EffectKind` (`"ReadSnapshot"`, `"ApplyPatch"`, ...).
fn kind_name(kind: &Value) -> Option<&str> {
    kind.as_str().or_else(|| kind.as_object().and_then(|m| m.keys().next()).map(String::as_str))
}

/// The first effect of kind `kind_variant` intended in `events`, optionally for `request`.
pub(crate) fn intended(events: &[StoredEvent], kind_variant: &str, request: Option<&Digest>) -> Result<Option<EffectId>> {
    for e in events.iter().filter(|e| e.event_type == "EffectIntended") {
        if kind_name(&e.payload["kind"]) != Some(kind_variant) {
            continue;
        }
        if let Some(request) = request {
            if decode::<Digest>(&e.payload["request_digest"])? != *request {
                continue;
            }
        }
        return Ok(Some(decode(&e.payload["effect_id"])?));
    }
    Ok(None)
}

/// The first `Denied` audit in `events`.
pub(crate) fn denial(events: &[StoredEvent]) -> Option<&Value> {
    events.iter().find(|e| e.event_type == "Denied").map(|e| &e.payload)
}

/// The observation a patch denial gave the agent, rebuilt from its audit payload.
pub(crate) fn denial_observation(payload: &Value) -> Result<Observation> {
    Ok(match payload["reason"].as_str() {
        Some("VersionConflict") => {
            Observation::VersionConflict { expected: decode(&payload["expected"])?, actual: decode(&payload["actual"])? }
        }
        Some("CapabilityDenied") => {
            let cap: Capability = decode(&payload["capability"])?;
            Observation::PatchRejected { reason: format!("capability {cap:?} not granted") }
        }
        _ => Observation::PatchRejected { reason: payload.to_string() },
    })
}

/// The text of a journaled `ApplyPatch` action whose digest is `request`, from any session.
pub(crate) fn journaled_patch(db: &Db, task: &TaskId, request: &Digest) -> Result<Option<String>> {
    for e in db.events(task)?.iter().filter(|e| e.event_type == "AgentTurn") {
        if let AgentAction::ApplyPatch(patch) = decode(&e.payload["action"])? {
            if Digest::of(patch.as_bytes()) == *request {
                return Ok(Some(patch));
            }
        }
    }
    Ok(None)
}

/// Whether a receipt of attempt `attempt` was already journaled as ignored or rejected.
pub(crate) fn receipt_audited(db: &Db, task: &TaskId, attempt: &Value) -> Result<bool> {
    Ok(db
        .events(task)?
        .iter()
        .any(|e| matches!(e.event_type.as_str(), "ReceiptIgnored" | "ReceiptRejected" | "RetainedReceiptRejected") && e.payload["receipt"]["attempt_id"] == *attempt))
}

fn protocol(rec: &EffectRecord, what: &str) -> EngineError {
    EngineError::Protocol(format!("result of effect {} {what}", rec.effect_id))
}

/// The observation a finished effect gives the agent, read from its published result, so a
/// live run and a resumed one see exactly the same thing.
///
/// For verification, the evidence must be for the workspace the effect was intended for:
/// the RunVerification request digest is the digest of that workspace digest, and the
/// workspace cannot change while the task is VERIFYING.
pub(crate) fn effect_observation(blobs: &BlobStore, rec: &EffectRecord) -> Result<Observation> {
    let digest = rec.result_digest.ok_or_else(|| protocol(rec, "was never published"))?;
    let result: Value = serde_json::from_slice(&blobs.get(&digest)?).map_err(DbError::from)?;
    let reason = || result["reason"].as_str().map(str::to_string).ok_or_else(|| protocol(rec, "has no failure reason"));
    Ok(match (&rec.kind, rec.state) {
        (EffectKind::ApplyPatch { .. }, EffectState::Completed) => {
            let workspace = result.get("workspace_digest").ok_or_else(|| protocol(rec, "has no workspace digest"))?;
            Observation::PatchApplied { workspace: decode(workspace)? }
        }
        (EffectKind::ApplyPatch { .. }, EffectState::Failed) => Observation::PatchRejected { reason: reason()? },
        (EffectKind::RunVerification, EffectState::Completed) => {
            let checked: Digest = decode(&result["workspace_digest"])?;
            if Digest::of(checked.as_bytes()) != rec.request_digest {
                let summary = format!("evidence is for workspace {checked}, not the one verified");
                Observation::Verification { passed: false, summary }
            } else {
                let summary = match result["summary"].as_str() {
                    Some(s) => s.to_string(),
                    None => format!("exit code {}", result["exit_code"]),
                };
                Observation::Verification { passed: result["passed"] == true, summary }
            }
        }
        (EffectKind::RunVerification, EffectState::Failed) => {
            Observation::Verification { passed: false, summary: format!("verification did not complete: {}", reason()?) }
        }
        (kind, state) => return Err(protocol(rec, &format!("({kind:?}, {state:?}) gives the agent no observation"))),
    })
}
