//! Durable per-attempt retention of outcomes produced in the controller (model responses,
//! analysis reports): `<root>/<effect>-<attempt>/<file>`, written before the outcome is
//! returned, so recovery publishes it instead of performing the effect again.

use std::io::ErrorKind;
use std::path::PathBuf;

use agentos_core::effect::{AttemptId, EffectId};

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome};
use crate::job::{atomic_write, check_plain_name, sync_dir};

pub(crate) struct Retention {
    pub root: PathBuf,
    /// The file holding the outcome inside an attempt's directory.
    pub file: &'static str,
    /// What is retained, for log messages ("model response").
    pub what: &'static str,
}

impl Retention {
    /// Where one attempt's outcome is retained: `<root>/<effect>-<attempt>`.
    pub fn dir(&self, effect: &EffectId, attempt: &AttemptId) -> PathBuf {
        self.root.join(format!("{effect}-{attempt}"))
    }

    pub fn retain(&self, req: &EffectRequest, ctx: &AttemptCtx, out: &ExecOutcome) {
        if let Err(e) = self.try_retain(req, ctx, out) {
            tracing::warn!(effect = %req.effect_id, error = %e, "cannot retain the {}", self.what);
        }
    }

    /// Writes the outcome durably: the file and its directory are synced by `atomic_write`;
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
        let dir = self.dir(&req.effect_id, &ctx.attempt_id);
        std::fs::create_dir_all(&dir)?;
        atomic_write(
            &dir.join(self.file),
            &serde_json::to_vec(out).expect("an outcome serializes"),
        )?;
        sync_dir(&self.root)?;
        if created_root && let Some(parent) = self.root.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    /// The newest-lease resolved outcome retained for `effect`, if any. Anything this cannot
    /// read or parse is logged (`warn`) and counts as not retained: the caller then treats
    /// the effect as lost, which is the safe side (it is never performed again on a guess).
    ///
    /// Cost: one listing of the retention directory per call; only entries named
    /// `<effect>-<attempt>` are opened.
    pub fn retained(&self, effect: &EffectId) -> Option<ExecOutcome> {
        let prefix = format!("{effect}-");
        let mut best: Option<ExecOutcome> = None;
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(root = %self.root.display(), error = %e,
                    "cannot list the {} retention directory; treating nothing as retained", self.what);
                return None;
            }
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            let file = entry.path().join(self.file);
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(%effect, file = %file.display(), error = %e,
                        "cannot read a retained {}; treating it as not retained", self.what);
                    continue;
                }
            };
            let out = match serde_json::from_slice::<ExecOutcome>(&bytes) {
                Ok(out) => out,
                Err(e) => {
                    tracing::warn!(%effect, file = %file.display(), error = %e,
                        "cannot parse a retained {}; treating it as not retained", self.what);
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
