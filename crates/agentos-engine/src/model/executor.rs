//! Runs a `ModelCall` effect: sends the request exactly once and retains the answer
//! before returning it, so recovery publishes the answer instead of sending again.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use serde_json::Value;

use super::provider::{ModelProvider, ProviderResult};
use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::guestlink::guest_text;
use crate::job::atomic_write;
use crate::supervised::ExecCounts;

/// How much of an unusable response body an error quotes (it is bounded again by
/// `guest_text`).
const EXCERPT: usize = 256;

pub struct ModelExecutor {
    root: PathBuf,
    provider: Option<Box<dyn ModelProvider>>,
    counts: ExecCounts,
    crash: Option<CrashHook>,
}

impl ModelExecutor {
    pub fn new(root: PathBuf, provider: Option<Box<dyn ModelProvider>>, counts: ExecCounts) -> ModelExecutor {
        ModelExecutor { root, provider, counts, crash: None }
    }

    pub fn with_crash(mut self, hook: Option<CrashHook>) -> Self {
        self.crash = hook;
        self
    }

    /// Where the answer of one attempt is retained: `<root>/<effect>-<attempt>`.
    pub fn retention_dir(&self, effect: &EffectId, attempt: &AttemptId) -> PathBuf {
        self.root.join(format!("{effect}-{attempt}"))
    }

    fn retain(&self, req: &EffectRequest, ctx: &AttemptCtx, out: &ExecOutcome) {
        let dir = self.retention_dir(&req.effect_id, &ctx.attempt_id);
        let result = std::fs::create_dir_all(&dir)
            .and_then(|()| atomic_write(&dir.join("response.json"), &serde_json::to_vec(out).expect("an outcome serializes")));
        if let Err(e) = result {
            tracing::warn!(effect = %req.effect_id, error = %e, "cannot retain the model response");
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Whether `bytes` look like a Messages response: an object with a `content` array and a
/// string `stop_reason`.
fn well_formed(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).is_ok_and(|v| v["content"].is_array() && v["stop_reason"].is_string())
}

impl Executor for ModelExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        if !matches!(req.kind, EffectKind::ModelCall { .. }) {
            return ExecOutcome::failure(req, ctx, "not a model call");
        }
        let Some(provider) = &self.provider else {
            return ExecOutcome::failure(req, ctx, "no model provider configured");
        };
        if req.deadline_ts != 0 && now() >= req.deadline_ts {
            return ExecOutcome::failure(req, ctx, "deadline exceeded");
        }
        let out = match provider.complete(&req.payload).await {
            ProviderResult::Response(bytes, _usage) if well_formed(&bytes) => ExecOutcome::success(req, ctx, bytes),
            ProviderResult::Response(bytes, _usage) => {
                let excerpt = String::from_utf8_lossy(&bytes[..bytes.len().min(EXCERPT)]).into_owned();
                ExecOutcome::failure(req, ctx, format!("malformed model response: {}", guest_text(&excerpt)))
            }
            ProviderResult::Rejected { status, body } => {
                ExecOutcome::failure(req, ctx, format!("http {status}: {}", guest_text(&body)))
            }
            ProviderResult::Transport(why) => {
                ExecOutcome::unresolved(req, ctx, format!("transport failure: {}", guest_text(&why)))
            }
        };
        self.counts.record(&req.kind);
        if self.crash.as_ref().is_some_and(|h| h.check(CrashPoint::DuringExecute, Some("model_call"))) {
            return ExecOutcome::failure(req, ctx, "injected crash before the response was retained");
        }
        if !out.unresolved {
            self.retain(req, ctx, &out);
        }
        out
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        let prefix = format!("{effect}-");
        let mut best: Option<ExecOutcome> = None;
        for entry in std::fs::read_dir(&self.root).ok()?.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path().join("response.json")) else { continue };
            let Ok(out) = serde_json::from_slice::<ExecOutcome>(&bytes) else { continue };
            if out.receipt.effect_id != *effect || out.unresolved {
                continue;
            }
            if best.as_ref().is_none_or(|b| out.receipt.lease_generation > b.receipt.lease_generation) {
                best = Some(out);
            }
        }
        best
    }
}
