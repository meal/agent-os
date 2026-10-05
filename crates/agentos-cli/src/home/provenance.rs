//! Parsing shared by CLI and bounded web reads of the first Submitted event.
use super::{Home, RecordedWorker, validate_base_url};
use crate::args::WorkerKind;
use crate::error::CliError;
use agentos_core::ids::{Digest, TaskId};
use serde_json::Value;

pub(crate) fn worker(task: &TaskId, submitted: Option<&Value>) -> Result<RecordedWorker, CliError> {
    let host = RecordedWorker {
        kind: WorkerKind::Host,
        image: None,
        jailed: None,
    };
    let Some(p) = submitted else { return Ok(host) };
    match p.get("worker").and_then(Value::as_str) {
        None | Some("host") => Ok(host),
        Some("firecracker") => {
            let broken = |what: &str| {
                CliError::other(format!(
                    "task {task} was submitted to the firecracker worker without a valid recorded {what}"
                ))
            };
            let id = p["guest_image_id"]
                .as_str()
                .ok_or_else(|| broken("guest_image_id"))?
                .to_owned();
            let digest = p["guest_image_digest"]
                .as_str()
                .and_then(|d| Digest::from_hex(d).ok())
                .ok_or_else(|| broken("guest_image_digest"))?;
            let jailed = p["jailed"].as_bool().ok_or_else(|| broken("jailed"))?;
            Ok(RecordedWorker {
                kind: WorkerKind::Firecracker,
                image: Some((id, digest)),
                jailed: Some(jailed),
            })
        }
        Some(other) => Err(CliError::other(format!(
            "task {task} was submitted with an unknown worker {other:?}"
        ))),
    }
}
pub(crate) fn model(submitted: Option<&Value>) -> Option<String> {
    submitted
        .and_then(|p| p["model"].as_str())
        .map(str::to_owned)
}
pub(crate) fn endpoint(submitted: Option<&Value>) -> Result<Option<String>, CliError> {
    if !model(submitted).is_some_and(|m| m.starts_with("anthropic:")) {
        return Ok(None);
    }
    let endpoint = submitted
        .and_then(|p| p["model_endpoint"].as_str())
        .unwrap_or(agentos_engine::model::anthropic::ANTHROPIC_BASE_URL);
    Ok(Some(
        validate_base_url(endpoint)?
            .trim_end_matches('/')
            .to_owned(),
    ))
}
// Home remains the configuration owner; this module never reads a provider key.
impl Home {
    pub(crate) fn recorded_payload(
        &self,
        db: &agentos_store::db::Db,
        task: &TaskId,
        max_bytes: Option<u64>,
    ) -> Result<Option<Value>, CliError> {
        Ok(db
            .first_event_bounded(task, "Submitted", max_bytes)?
            .map(|e| e.payload))
    }
}
