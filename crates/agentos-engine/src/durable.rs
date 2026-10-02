//! Durable executor receipts: a stand-in for the Phase 3 supervisor, which keeps every
//! outcome it produced on its own disk, independent of the controller's database.
//!
//! [`DurableExecutor`] wraps any executor and writes each outcome to
//! `<receipt_dir>/<effect_id>-<attempt_id>.json` (write temp file, fsync, rename, fsync the
//! directory) BEFORE returning it, so once the controller has seen an outcome, a restarted
//! controller can find it again through [`Executor::retained_outcome`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use agentos_core::effect::EffectId;
use agentos_core::ids::{Digest, TaskId};

use crate::crash::{CrashHook, CrashPoint};
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, Reconciliation};
pub use crate::supervised::ExecCounts;

pub struct DurableExecutor<E> {
    inner: E,
    dir: PathBuf,
    counts: ExecCounts,
    crash: Option<CrashHook>,
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

impl<E: Executor> DurableExecutor<E> {
    pub fn new(inner: E, receipt_dir: PathBuf, counts: ExecCounts) -> io::Result<DurableExecutor<E>> {
        fs::create_dir_all(&receipt_dir)?;
        Ok(DurableExecutor { inner, dir: receipt_dir, counts, crash: None })
    }

    /// Consults `hook` at `CrashPoint::DuringExecute`: when it fires, the outcome is
    /// returned without being persisted. Pass a clone of the hook the run uses, so the
    /// runner sees the crash.
    pub fn with_crash(mut self, hook: Option<CrashHook>) -> DurableExecutor<E> {
        self.crash = hook;
        self
    }

    pub fn inner(&self) -> &E {
        &self.inner
    }

    pub fn counts(&self) -> &ExecCounts {
        &self.counts
    }

    fn persist(&self, out: &ExecOutcome) -> io::Result<()> {
        let name = format!("{}-{}.json", out.receipt.effect_id, out.receipt.attempt_id);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let bytes = serde_json::to_vec(out).map_err(io::Error::other)?;
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, self.dir.join(name))?;
        sync_dir(&self.dir)
    }

    /// Persists `out`. A failure is logged, not hidden in the outcome: the effect did
    /// happen, and the controller records the receipt in its own journal anyway; without
    /// the retained copy recovery falls back to reconciling or retrying.
    fn retain(&self, out: &ExecOutcome) {
        if let Err(e) = self.persist(out) {
            tracing::warn!(effect_id = %out.receipt.effect_id, error = %e, "could not retain receipt");
        }
    }
}

impl<E: Executor + Sync> Executor for DurableExecutor<E> {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        self.counts.record(&req.kind);
        let out = self.inner.run(req, ctx).await;
        if self.crash.as_ref().is_some_and(|h| h.check(CrashPoint::DuringExecute, Some(req.kind.tag()))) {
            // Killed after the side effects, before the receipt became durable.
            return out;
        }
        self.retain(&out);
        out
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        let prefix = format!("{effect}-");
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list retained receipts");
                return None;
            }
        };
        let mut best: Option<ExecOutcome> = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            let parsed = fs::read(entry.path())
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice::<ExecOutcome>(&b).map_err(|e| e.to_string()));
            match parsed {
                Ok(out) if out.receipt.effect_id == *effect => {
                    if best.as_ref().is_none_or(|b| out.receipt.lease_generation > b.receipt.lease_generation) {
                        best = Some(out);
                    }
                }
                Ok(_) => tracing::warn!(file = name, "retained receipt names another effect"),
                Err(e) => tracing::warn!(file = name, error = %e, "unreadable retained receipt"),
            }
        }
        best
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        let found = self.inner.reconcile(req, ctx).await;
        if let Reconciliation::Applied(out) = &found {
            // Retained like any receipt, so a second recovery need not reconcile again.
            self.retain(out);
        }
        found
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.inner.current_workspace(task)
    }
}
