//! Runs a `ModelCall` effect: sends the request exactly once and retains the answer
//! before returning it, so recovery publishes the answer instead of sending again.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::Digest;
use serde_json::Value;

use super::policy::{ModelFailure, ModelFailureClass, classify};
use super::provider::{ModelProvider, ProviderResult};
use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::guestlink::guest_text;
use crate::retention::Retention;
use crate::supervised::ExecCounts;

/// How much of an unusable response body an error quotes (it is bounded again by
/// `guest_text`).
const EXCERPT: usize = 256;

pub struct ModelExecutor {
    retention: Retention,
    provider: Option<Box<dyn ModelProvider>>,
    counts: ExecCounts,
    crash: Option<CrashHook>,
}

impl ModelExecutor {
    pub fn new(
        root: PathBuf,
        provider: Option<Box<dyn ModelProvider>>,
        counts: ExecCounts,
    ) -> ModelExecutor {
        ModelExecutor {
            retention: Retention {
                root,
                file: "response.json",
                what: "model response",
            },
            provider,
            counts,
            crash: None,
        }
    }

    pub fn with_crash(mut self, hook: Option<CrashHook>) -> Self {
        self.crash = hook;
        self
    }

    /// Where the answer of one attempt is retained: `<root>/<effect>-<attempt>`.
    pub fn retention_dir(&self, effect: &EffectId, attempt: &AttemptId) -> PathBuf {
        self.retention.dir(effect, attempt)
    }
}

fn deadline_remaining(deadline_ts: i64, at: SystemTime) -> Option<Duration> {
    let seconds = u64::try_from(deadline_ts).ok()?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))?
        .duration_since(at)
        .ok()
        .filter(|remaining| !remaining.is_zero())
}

/// Whether `bytes` look like a Messages response: an object with a `content` array and a
/// string `stop_reason`.
fn well_formed(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .is_ok_and(|v| v["content"].is_array() && v["stop_reason"].is_string())
}

fn classified_failure(
    req: &EffectRequest,
    ctx: &AttemptCtx,
    reason: String,
    failure: ModelFailure,
) -> ExecOutcome {
    let mut out = ExecOutcome::failure(req, ctx, reason);
    let mut value: Value = serde_json::from_slice(&out.output).expect("failure JSON");
    value["failure"] = serde_json::to_value(failure).expect("failure metadata");
    out.output = serde_json::to_vec(&value).expect("failure JSON");
    out.receipt.result_digest = Some(Digest::of(&out.output));
    out
}

impl Executor for ModelExecutor {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        if !matches!(req.kind, EffectKind::ModelCall { .. }) {
            return ExecOutcome::failure(req, ctx, "not a model call");
        }
        let Some(provider) = &self.provider else {
            return ExecOutcome::failure(req, ctx, "no model provider configured");
        };
        let result = if req.deadline_ts == 0 {
            provider.complete(&req.payload).await
        } else {
            let Some(remaining) = deadline_remaining(req.deadline_ts, SystemTime::now()) else {
                return ExecOutcome::failure(req, ctx, "deadline exceeded");
            };
            match tokio::time::timeout(remaining, provider.complete(&req.payload)).await {
                Ok(result) => result,
                Err(_) => {
                    ProviderResult::Transport("task deadline exceeded during model call".into())
                }
            }
        };
        let out = match result {
            ProviderResult::Response(bytes, _usage) if well_formed(&bytes) => {
                ExecOutcome::success(req, ctx, bytes)
            }
            ProviderResult::Response(bytes, _usage) => {
                let excerpt =
                    String::from_utf8_lossy(&bytes[..bytes.len().min(EXCERPT)]).into_owned();
                classified_failure(
                    req,
                    ctx,
                    format!("malformed model response: {}", guest_text(&excerpt)),
                    ModelFailure {
                        class: ModelFailureClass::Permanent,
                        retry_not_before_ts: None,
                    },
                )
            }
            ProviderResult::Rejected { status, body } => classified_failure(
                req,
                ctx,
                format!("http {status}: {}", guest_text(&body)),
                ModelFailure {
                    class: classify(status),
                    retry_not_before_ts: None,
                },
            ),
            ProviderResult::RejectedWithRetryAfter {
                status,
                body,
                retry_not_before_ts,
            } => classified_failure(
                req,
                ctx,
                format!("http {status}: {}", guest_text(&body)),
                ModelFailure {
                    class: classify(status),
                    retry_not_before_ts: Some(retry_not_before_ts),
                },
            ),
            ProviderResult::Transport(why) => ExecOutcome::unresolved(
                req,
                ctx,
                format!("transport failure: {}", guest_text(&why)),
            ),
        };
        self.counts.record(&req.kind);
        if self
            .crash
            .as_ref()
            .is_some_and(|h| h.check(CrashPoint::DuringExecute, Some("model_call")))
        {
            return ExecOutcome::failure(
                req,
                ctx,
                "injected crash before the response was retained",
            );
        }
        if !out.unresolved {
            self.retention.retain(req, ctx, &out);
        }
        out
    }

    /// The newest-lease resolved outcome retained for `effect` (see [`Retention::retained`]).
    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.retention.retained(effect)
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn remaining_duration_preserves_fractional_seconds() {
        let at = UNIX_EPOCH + Duration::from_millis(9_750);
        assert_eq!(deadline_remaining(10, at), Some(Duration::from_millis(250)));
    }

    #[test]
    fn elapsed_or_invalid_deadlines_have_no_remaining_duration() {
        let at = UNIX_EPOCH + Duration::from_secs(10);
        assert_eq!(deadline_remaining(10, at), None);
        assert_eq!(deadline_remaining(9, at), None);
        assert_eq!(deadline_remaining(-1, at), None);
    }
}
