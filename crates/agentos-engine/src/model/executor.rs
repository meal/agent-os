//! Runs a `ModelCall` effect: sends the request exactly once and retains the answer
//! before returning it, so recovery publishes the answer instead of sending again.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::Digest;
use serde_json::Value;

use super::policy::{ModelFailure, ModelFailureClass, classify};
use super::provider::{ModelProvider, ProviderResult};
use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use crate::guestlink::guest_text;
use crate::job::{atomic_write, check_plain_name, sync_dir};
use crate::supervised::ExecCounts;

/// How much of an unusable response body an error quotes (it is bounded again by
/// `guest_text`).
const EXCERPT: usize = 256;

/// A change of the retention root younger than this at the time of a listing might share its
/// timestamp with a later change (file systems stamp directories coarsely), so a listing
/// that young is not trusted to prove that nothing else appeared.
const MTIME_TRUST_AFTER: Duration = Duration::from_secs(2);

/// The retention root's entries by effect id, as of one listing (names only: no file is
/// opened to build it).
#[derive(Default)]
struct RetentionIndex {
    by_effect: HashMap<String, Vec<PathBuf>>,
    /// `(root mtime, when listed)` of the last listing; `None` before the first.
    listed: Option<(Option<SystemTime>, SystemTime)>,
}

pub struct ModelExecutor {
    root: PathBuf,
    provider: Option<Box<dyn ModelProvider>>,
    counts: ExecCounts,
    crash: Option<CrashHook>,
    /// Effect id -> its retention directories; see [`ModelExecutor::retained_outcome`].
    index: Mutex<RetentionIndex>,
    /// How many times the retention directory was listed.
    scans: AtomicUsize,
    /// How many `response.json` files were opened by `retained_outcome`.
    response_reads: AtomicUsize,
}

impl ModelExecutor {
    pub fn new(
        root: PathBuf,
        provider: Option<Box<dyn ModelProvider>>,
        counts: ExecCounts,
    ) -> ModelExecutor {
        ModelExecutor {
            root,
            provider,
            counts,
            crash: None,
            index: Mutex::new(RetentionIndex::default()),
            scans: AtomicUsize::new(0),
            response_reads: AtomicUsize::new(0),
        }
    }

    /// How many times `retained_outcome` listed the retention directory (observability).
    pub fn directory_scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }

    /// How many `response.json` files `retained_outcome` opened (observability).
    pub fn response_reads(&self) -> usize {
        self.response_reads.load(Ordering::SeqCst)
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
        if let Err(e) = self.try_retain(req, ctx, out) {
            tracing::warn!(effect = %req.effect_id, error = %e, "cannot retain the model response");
        }
    }

    /// Writes the answer durably: the file and its directory are synced by `atomic_write`;
    /// the directory's entry in `root` (and `root`'s own entry, when this created it) are
    /// synced here, so a power loss cannot leave a synced file under a vanished directory.
    fn try_retain(
        &self,
        req: &EffectRequest,
        ctx: &AttemptCtx,
        out: &ExecOutcome,
    ) -> std::io::Result<()> {
        check_plain_name("effect id", req.effect_id.as_str())?;
        let attempt = ctx.attempt_id.to_string();
        check_plain_name("attempt id", &attempt)?;
        let created_root = !self.root.exists();
        std::fs::create_dir_all(&self.root)?;
        let dir = self.retention_dir(&req.effect_id, &ctx.attempt_id);
        std::fs::create_dir_all(&dir)?;
        atomic_write(
            &dir.join("response.json"),
            &serde_json::to_vec(out).expect("an outcome serializes"),
        )?;
        sync_dir(&self.root)?;
        if created_root && let Some(parent) = self.root.parent() {
            sync_dir(parent)?;
        }
        self.note_retained(req.effect_id.as_str(), dir);
        Ok(())
    }

    /// Records a directory this executor just wrote, so the index stays complete without a
    /// new listing.
    fn note_retained(&self, effect: &str, dir: PathBuf) {
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        if index.listed.is_none() {
            return; // the first lookup will list everything, this directory included
        }
        let dirs = index.by_effect.entry(effect.to_string()).or_default();
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
        let mtime = self.root_mtime();
        if let Some((listed_mtime, _)) = &mut index.listed {
            *listed_mtime = mtime;
        }
    }

    fn root_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.root)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Lists the root once, by name: `<effect>-<attempt>` entries grouped by effect id (an
    /// effect id is hex, so it ends at the first `-`). Nothing inside an entry is read.
    fn list_root(&self) -> RetentionIndex {
        self.scans.fetch_add(1, Ordering::SeqCst);
        let at = SystemTime::now();
        let mtime = self.root_mtime();
        let mut by_effect: HashMap<String, Vec<PathBuf>> = HashMap::new();
        match std::fs::read_dir(&self.root) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if let Some((effect, _attempt)) = name.split_once('-') {
                                by_effect
                                    .entry(effect.to_string())
                                    .or_default()
                                    .push(entry.path());
                            }
                        }
                        Err(e) => tracing::warn!(
                            root = %self.root.display(), error = %e,
                            "cannot read an entry of the model retention directory"
                        ),
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                root = %self.root.display(), error = %e,
                "cannot list the model retention directory; treating nothing as retained"
            ),
        }
        RetentionIndex {
            by_effect,
            listed: Some((mtime, at)),
        }
    }

    /// The retention directories of `effect`. The root is listed (names only) on the first
    /// call and again whenever its mtime changed since, or when `effect` is not indexed and the
    /// last listing is too young for its mtime to prove that nothing was added. Otherwise a
    /// lookup costs one `stat` of the root.
    fn retention_dirs(&self, effect: &EffectId) -> Vec<PathBuf> {
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let mtime = self.root_mtime();
        let stale = match index.listed {
            None => true,
            Some((listed_mtime, _)) => listed_mtime != mtime,
        };
        let hit = |index: &RetentionIndex| index.by_effect.get(effect.as_str()).cloned();
        if !stale && let Some(dirs) = hit(&index) {
            return dirs;
        }
        let young = match (mtime, index.listed) {
            (Some(m), Some((_, at))) => at.duration_since(m).is_ok_and(|d| d < MTIME_TRUST_AFTER),
            _ => true,
        };
        if stale || young {
            *index = self.list_root();
        }
        hit(&index).unwrap_or_default()
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
            self.retain(req, ctx, &out);
        }
        out
    }

    /// The newest-lease resolved outcome retained for `effect`, if any. Anything this cannot
    /// read or parse is logged (`warn`) and counts as not retained: the caller then treats
    /// the effect as lost, which is the safe side (it is never sent again on a guess).
    ///
    /// Cost: the retention directory is indexed by effect id, so a lookup opens only that
    /// effect's own `response.json` files. The index is a listing of entry names (O(entries),
    /// no file opened), made on the first call, whenever the root's mtime changed since, and
    /// on a miss while the last listing is under two seconds old (timestamps are coarse); in
    /// between, a lookup is one `stat` plus a map probe. The index also assumes this executor
    /// is the only writer, as it is under the driver lock; entries it retains itself are added
    /// as they are written.
    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        let mut best: Option<ExecOutcome> = None;
        for dir in self.retention_dirs(effect) {
            let file = dir.join("response.json");
            self.response_reads.fetch_add(1, Ordering::SeqCst);
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(%effect, file = %file.display(), error = %e,
                        "cannot read a retained model response; treating it as not retained");
                    continue;
                }
            };
            let out = match serde_json::from_slice::<ExecOutcome>(&bytes) {
                Ok(out) => out,
                Err(e) => {
                    tracing::warn!(%effect, file = %file.display(), error = %e,
                        "cannot parse a retained model response; treating it as not retained");
                    continue;
                }
            };
            if out.receipt.effect_id != *effect || out.unresolved {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|b| out.receipt.lease_generation > b.receipt.lease_generation)
            {
                best = Some(out);
            }
        }
        best
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
