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
use serde_json::{Value, json};

use crate::agent::{AgentAction, Observation};
use crate::runner::{EngineError, Result};

pub(crate) struct Turn {
    pub seq: u64,
    pub turn: u32,
    pub observation: Observation,
    pub action: AgentAction,
}

/// Appends the turn the runner is about to act on; returns its journal sequence number.
pub(crate) fn append_turn(
    db: &Db,
    task: &TaskId,
    turn: u32,
    obs: &Observation,
    action: &AgentAction,
) -> Result<u64> {
    let payload = json!({ "turn": turn, "observation": obs, "action": action });
    Ok(db.append_audit(task, "AgentTurn", &payload)?)
}

/// Journals one model request a guest session sent: `id` is the CLI's request number,
/// `request` the digest of the body the runner sent, `requested_model` the model the CLI asked
/// for and `model` the one it was sent to. An audit event: `session_turns` never reads it.
// The session runner journals through it; until then nothing calls it.
#[allow(dead_code)]
pub(crate) fn append_session_call(
    db: &Db,
    task: &TaskId,
    id: u64,
    request: &Digest,
    requested_model: &str,
    model: &str,
) -> Result<u64> {
    let payload = json!({
        "id": id,
        "request": request,
        "requested_model": requested_model,
        "model": model,
    });
    Ok(db.append_audit(task, "SessionModelCall", &payload)?)
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
    kind.as_str().or_else(|| {
        kind.as_object()
            .and_then(|m| m.keys().next())
            .map(String::as_str)
    })
}

/// The first effect of kind `kind_variant` intended in `events`, optionally for `request`.
pub(crate) fn intended(
    events: &[StoredEvent],
    kind_variant: &str,
    request: Option<&Digest>,
) -> Result<Option<EffectId>> {
    for e in events.iter().filter(|e| e.event_type == "EffectIntended") {
        if kind_name(&e.payload["kind"]) != Some(kind_variant) {
            continue;
        }
        if let Some(request) = request
            && decode::<Digest>(&e.payload["request_digest"])? != *request
        {
            continue;
        }
        return Ok(Some(decode(&e.payload["effect_id"])?));
    }
    Ok(None)
}

/// The first `Denied` audit in `events`.
pub(crate) fn denial(events: &[StoredEvent]) -> Option<&Value> {
    events
        .iter()
        .find(|e| e.event_type == "Denied")
        .map(|e| &e.payload)
}

/// The observation a patch denial gave the agent, rebuilt from its audit payload.
pub(crate) fn denial_observation(payload: &Value) -> Result<Observation> {
    if matches!(payload["action"].as_str(), Some("ReadFile" | "ListFiles")) {
        return Ok(Observation::FileReadRejected {
            reason: payload.to_string(),
        });
    }
    Ok(match payload["reason"].as_str() {
        Some("VersionConflict") => Observation::VersionConflict {
            expected: decode(&payload["expected"])?,
            actual: decode(&payload["actual"])?,
        },
        Some("CapabilityDenied") if kind_name(&payload["kind"]) == Some("ModelCall") => {
            Observation::ModelCallFailed {
                reason: "capability model.request not granted".into(),
                failure: None,
            }
        }
        Some("CapabilityDenied")
            if matches!(kind_name(&payload["kind"]), Some("ListFiles" | "ReadFile")) =>
        {
            Observation::FileReadRejected {
                reason: "capability snapshot.read not granted".into(),
            }
        }
        Some("CapabilityDenied") => {
            let cap: Capability = decode(&payload["capability"])?;
            Observation::PatchRejected {
                reason: format!("capability {cap:?} not granted"),
            }
        }
        _ => Observation::PatchRejected {
            reason: payload.to_string(),
        },
    })
}

/// The text of a journaled `ApplyPatch` action whose digest is `request`, from any session.
pub(crate) fn journaled_patch(db: &Db, task: &TaskId, request: &Digest) -> Result<Option<String>> {
    for e in db
        .events(task)?
        .iter()
        .filter(|e| e.event_type == "AgentTurn")
    {
        if let AgentAction::ApplyPatch(patch) = decode(&e.payload["action"])?
            && Digest::of(patch.as_bytes()) == *request
        {
            return Ok(Some(patch));
        }
    }
    Ok(None)
}

/// The texts of the task's COMPLETED `ApplyPatch` effects, in intent order, each taken from
/// its journaled `AgentTurn` (the one place every patch is kept; the reader that replays
/// them holds no blob store). A completed patch whose text is not journaled is an error: the
/// replay must not guess.
pub(crate) fn completed_patches(db: &Db, task: &TaskId) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for e in db
        .events(task)?
        .iter()
        .filter(|e| e.event_type == "EffectIntended")
    {
        if kind_name(&e.payload["kind"]) != Some("ApplyPatch") {
            continue;
        }
        let id: EffectId = decode(&e.payload["effect_id"])?;
        let rec = db.effect(&id)?;
        if rec.state != EffectState::Completed {
            continue;
        }
        match journaled_patch(db, task, &rec.request_digest)? {
            Some(text) => out.push(text.into_bytes()),
            None => {
                return Err(EngineError::Protocol(format!(
                    "the patch of effect {id} is not journaled"
                )));
            }
        }
    }
    Ok(out)
}

/// Whether a receipt of attempt `attempt` was already journaled as ignored or rejected.
pub(crate) fn receipt_audited(db: &Db, task: &TaskId, attempt: &Value) -> Result<bool> {
    Ok(db.events(task)?.iter().any(|e| {
        matches!(
            e.event_type.as_str(),
            "ReceiptIgnored" | "ReceiptRejected" | "RetainedReceiptRejected"
        ) && e.payload["receipt"]["attempt_id"] == *attempt
    }))
}

/// The serialized request of a `ModelCall` effect: the blob at its request digest, if it
/// still exists.
pub(crate) fn model_request_body(blobs: &BlobStore, rec: &EffectRecord) -> Result<Option<Vec<u8>>> {
    if !blobs.exists(&rec.request_digest) {
        return Ok(None);
    }
    Ok(Some(blobs.get(&rec.request_digest)?))
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
    // A forfeited model call has no result by design: checked before the blob read.
    if let (EffectKind::ModelCall { .. }, EffectState::Failed, None) =
        (&rec.kind, rec.state, rec.result_digest)
    {
        return Ok(Observation::ModelCallLost);
    }
    let digest = rec
        .result_digest
        .ok_or_else(|| protocol(rec, "was never published"))?;
    let result: Value = serde_json::from_slice(&blobs.get(&digest)?).map_err(DbError::from)?;
    let reason = || {
        result["reason"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| protocol(rec, "has no failure reason"))
    };
    Ok(match (&rec.kind, rec.state) {
        (EffectKind::ApplyPatch { .. }, EffectState::Completed) => {
            let workspace = result
                .get("workspace_digest")
                .ok_or_else(|| protocol(rec, "has no workspace digest"))?;
            Observation::PatchApplied {
                workspace: decode(workspace)?,
            }
        }
        (EffectKind::ApplyPatch { .. }, EffectState::Failed) => {
            Observation::PatchRejected { reason: reason()? }
        }
        (EffectKind::RunVerification, EffectState::Completed) => {
            let checked: Digest = decode(&result["workspace_digest"])?;
            if Digest::of(checked.as_bytes()) != rec.request_digest {
                let summary = format!("evidence is for workspace {checked}, not the one verified");
                Observation::Verification {
                    passed: false,
                    summary,
                }
            } else {
                let summary = match result["summary"].as_str() {
                    Some(s) => s.to_string(),
                    None => format!("exit code {}", result["exit_code"]),
                };
                Observation::Verification {
                    passed: result["passed"] == true,
                    summary,
                }
            }
        }
        (EffectKind::RunVerification, EffectState::Failed) => Observation::Verification {
            passed: false,
            summary: format!("verification did not complete: {}", reason()?),
        },
        (EffectKind::ModelCall { .. }, EffectState::Completed) => {
            let content = result["content"].clone();
            if !content.is_array() {
                return Err(protocol(rec, "has no content array"));
            }
            Observation::ModelResponse {
                content,
                stop_reason: result["stop_reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string(),
                output_tokens: result["usage"]["output_tokens"].as_u64().unwrap_or(0),
            }
        }
        (EffectKind::ModelCall { .. }, EffectState::Failed) => Observation::ModelCallFailed {
            reason: reason()?,
            failure: result.get("failure").map(decode).transpose()?,
        },
        (EffectKind::ListFiles { .. }, EffectState::Completed) => Observation::Files {
            files: decode(&result["files"])?,
        },
        (EffectKind::ReadFile { .. }, EffectState::Completed) => Observation::FileRead {
            path: decode(&result["path"])?,
            content: decode(&result["content"])?,
            truncated: result["truncated"] == true,
        },
        (EffectKind::ListFiles { .. } | EffectKind::ReadFile { .. }, EffectState::Failed) => {
            Observation::FileReadRejected { reason: reason()? }
        }
        (kind, state) => {
            return Err(protocol(
                rec,
                &format!("({kind:?}, {state:?}) gives the agent no observation"),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use agentos_core::contract::Contract;
    use agentos_core::ids::TaskId;
    use serde_json::json;

    use super::*;

    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    fn rec(kind: EffectKind, state: EffectState, result: Option<Digest>) -> EffectRecord {
        EffectRecord {
            effect_id: EffectId::derive(&TaskId::new(), 1, &kind, &d("r")),
            task_id: TaskId::new(),
            step: 1,
            kind,
            state,
            request_digest: d("r"),
            lease_generation: 1,
            result_digest: result,
        }
    }

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        (dir, blobs)
    }

    fn with_result(
        blobs: &BlobStore,
        kind: EffectKind,
        state: EffectState,
        result: Value,
    ) -> EffectRecord {
        let digest = blobs.put(result.to_string().as_bytes()).unwrap();
        rec(kind, state, Some(digest))
    }

    fn model() -> EffectKind {
        EffectKind::ModelCall {
            model: "m".into(),
            turn: 1,
        }
    }

    #[test]
    fn model_call_observations_come_from_the_response_blob_or_its_absence() {
        let (_dir, blobs) = store();
        let content = json!([{ "type": "text", "text": "hi" }]);
        let response = json!({ "content": content, "stop_reason": "end_turn", "usage": { "input_tokens": 1, "output_tokens": 7 } });
        let done = with_result(&blobs, model(), EffectState::Completed, response);
        assert_eq!(
            effect_observation(&blobs, &done).unwrap(),
            Observation::ModelResponse {
                content,
                stop_reason: "end_turn".into(),
                output_tokens: 7
            }
        );

        // Lost: no result at all, decided before the store is consulted.
        let (_other, empty) = store();
        let lost = rec(model(), EffectState::Failed, None);
        assert_eq!(
            effect_observation(&empty, &lost).unwrap(),
            Observation::ModelCallLost
        );

        let failed = with_result(
            &blobs,
            model(),
            EffectState::Failed,
            json!({ "reason": "http 400: bad" }),
        );
        assert_eq!(
            effect_observation(&blobs, &failed).unwrap(),
            Observation::ModelCallFailed {
                reason: "http 400: bad".into(),
                failure: None
            }
        );

        let bare = with_result(
            &blobs,
            model(),
            EffectState::Completed,
            json!({ "stop_reason": "end_turn" }),
        );
        assert!(matches!(
            effect_observation(&blobs, &bare),
            Err(EngineError::Protocol(_))
        ));

        let unnamed = with_result(
            &blobs,
            model(),
            EffectState::Completed,
            json!({ "content": [] }),
        );
        assert_eq!(
            effect_observation(&blobs, &unnamed).unwrap(),
            Observation::ModelResponse {
                content: json!([]),
                stop_reason: "unknown".into(),
                output_tokens: 0
            }
        );
    }

    #[test]
    fn read_observations_come_from_their_result_blobs() {
        let (_dir, blobs) = store();
        let ws = d("ws");
        let list = EffectKind::ListFiles { turn: 1 };
        let read = EffectKind::ReadFile {
            path: "a".into(),
            turn: 1,
        };
        let files = with_result(
            &blobs,
            list.clone(),
            EffectState::Completed,
            json!({ "files": ["a", "b"], "workspace_digest": ws }),
        );
        assert_eq!(
            effect_observation(&blobs, &files).unwrap(),
            Observation::Files {
                files: vec!["a".into(), "b".into()]
            }
        );
        let file = with_result(
            &blobs,
            read.clone(),
            EffectState::Completed,
            json!({ "path": "a", "content": "x", "truncated": true, "workspace_digest": ws }),
        );
        assert_eq!(
            effect_observation(&blobs, &file).unwrap(),
            Observation::FileRead {
                path: "a".into(),
                content: "x".into(),
                truncated: true
            }
        );
        for kind in [list, read] {
            let failed = with_result(
                &blobs,
                kind,
                EffectState::Failed,
                json!({ "reason": "no such file" }),
            );
            assert_eq!(
                effect_observation(&blobs, &failed).unwrap(),
                Observation::FileReadRejected {
                    reason: "no such file".into()
                }
            );
        }
    }

    #[test]
    fn denials_name_their_action() {
        let read = json!({ "action": "ReadFile", "reason": "InvalidPath", "path": "../x" });
        assert_eq!(
            denial_observation(&read).unwrap(),
            Observation::FileReadRejected {
                reason: read.to_string()
            }
        );
        let model_denied = json!({ "reason": "CapabilityDenied", "capability": "model.request", "kind": { "ModelCall": { "model": "m", "turn": 1 } } });
        assert_eq!(
            denial_observation(&model_denied).unwrap(),
            Observation::ModelCallFailed {
                reason: "capability model.request not granted".into(),
                failure: None
            }
        );
        for kind in [
            json!({ "ListFiles": { "turn": 1 } }),
            json!({ "ReadFile": { "path": "a", "turn": 1 } }),
        ] {
            let denied = json!({ "reason": "CapabilityDenied", "capability": "snapshot.read", "kind": kind });
            assert_eq!(
                denial_observation(&denied).unwrap(),
                Observation::FileReadRejected {
                    reason: "capability snapshot.read not granted".into()
                }
            );
        }
        // A non-capability denial journaled by a listing is a read rejection too, never a
        // rejected patch.
        let listed = json!({ "action": "ListFiles", "reason": "SomethingElse" });
        assert_eq!(
            denial_observation(&listed).unwrap(),
            Observation::FileReadRejected {
                reason: listed.to_string()
            }
        );
        // Patch denials are unchanged.
        let patch = json!({ "action": "ApplyPatch", "reason": "PathNotEditable", "paths": ["x"] });
        assert_eq!(
            denial_observation(&patch).unwrap(),
            Observation::PatchRejected {
                reason: patch.to_string()
            }
        );
        let conflict = json!({ "reason": "VersionConflict", "expected": d("a"), "actual": d("b") });
        assert_eq!(
            denial_observation(&conflict).unwrap(),
            Observation::VersionConflict {
                expected: d("a"),
                actual: d("b")
            }
        );
        let cap = json!({ "reason": "CapabilityDenied", "capability": "workspace.apply_patch", "kind": { "ApplyPatch": { "expected_base": d("a") } } });
        assert!(matches!(
            denial_observation(&cap).unwrap(),
            Observation::PatchRejected { .. }
        ));
    }

    /// The session's model calls are audit events between the agent's turns: `session_turns`
    /// reads only the turns, so the replay of the open session never sees them.
    #[test]
    fn session_model_calls_are_audit_events_that_session_turns_skips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let contract = Contract::parse(
            r#"{"goal": "g", "repository": {"source": "s", "revision": "r"}, "profile": "p",
                "editable_paths": ["src/**"], "verification_profile": "v", "capabilities": [],
                "limits": {"model_requests": 2, "max_output_tokens_per_request": 8, "tool_actions": 2,
                           "deadline_seconds": 60, "worker_vcpus": 1, "worker_memory_mib": 1}}"#,
        )
        .unwrap();
        let task = db.create_task(&contract, &d("c")).unwrap();
        let start = Observation::Start {
            files: vec![],
            workspace: d("w"),
        };
        let run = AgentAction::RunSession {
            argv: vec!["cli".into()],
            env: vec![],
            model: "claude-opus-5-5".into(),
        };
        append_turn(&db, &task, 1, &start, &run).unwrap();
        append_session_call(
            &db,
            &task,
            1,
            &d("r1"),
            "claude-opus-5-5-big",
            "claude-opus-5-5",
        )
        .unwrap();
        append_session_call(&db, &task, 2, &d("r2"), "claude-haiku", "claude-opus-5-5").unwrap();
        let ended = Observation::SessionEnded {
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            patch: Some("p".into()),
            reason: None,
        };
        append_turn(&db, &task, 2, &ended, &AgentAction::ApplyPatch("p".into())).unwrap();

        let events = db.events(&task).unwrap();
        let calls: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "SessionModelCall")
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].payload["id"], 1);
        assert_eq!(calls[0].payload["requested_model"], "claude-opus-5-5-big");
        assert_eq!(calls[0].payload["model"], "claude-opus-5-5");
        assert_eq!(calls[1].payload["id"], 2);
        let turns = session_turns(&events).unwrap();
        assert_eq!(
            turns.iter().map(|t| t.turn).collect::<Vec<_>>(),
            vec![1, 2],
            "only the AgentTurn events are turns"
        );
        assert_eq!(turns[1].action, AgentAction::ApplyPatch("p".into()));
    }
}
