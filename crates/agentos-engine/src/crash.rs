//! Crash injection. A [`CrashHook`] is consulted at every persistence and dispatch boundary
//! of the run loop and of recovery; when it fires, the run returns
//! `EngineError::Crashed(point)` at once, doing no cleanup, exactly as if the controller had
//! been killed there. Tests then drop every in-memory handle and reopen the same on-disk
//! state with fresh objects, which is all a restarted controller has.
//!
//! Without a hook ([`RunOptions::default`]) every check is a `None` test.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

/// The boundaries a crash can be injected at, in the order one effect passes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CrashPoint {
    /// The agent's action is chosen and journaled as an `AgentTurn`; nothing else is done.
    AfterAgentTurnJournaled,
    /// The intent and its reservation are committed; nothing is dispatched.
    AfterIntent,
    /// The effect is DISPATCHED; the executor has not been called.
    AfterDispatch,
    /// Inside the executor: the side effects happened, the receipt is not durable yet.
    DuringExecute,
    /// The receipt is durable in the executor's own log; nothing is in the blob store.
    AfterExecuteBeforePublish,
    /// The result bytes are in the blob store but not registered.
    AfterBlobPut,
    /// The result artifact is registered; the effect is not completed.
    AfterRegister,
    /// The completion and its follow-up task event are committed; the agent was not told.
    AfterComplete,
}

impl CrashPoint {
    pub const ALL: [CrashPoint; 8] = [
        CrashPoint::AfterAgentTurnJournaled,
        CrashPoint::AfterIntent,
        CrashPoint::AfterDispatch,
        CrashPoint::DuringExecute,
        CrashPoint::AfterExecuteBeforePublish,
        CrashPoint::AfterBlobPut,
        CrashPoint::AfterRegister,
        CrashPoint::AfterComplete,
    ];
}

impl fmt::Display for CrashPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// What the hook is told about the boundary being passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashCtx {
    /// Tag of the effect kind involved (`EffectKind::tag`), if any. For
    /// `AfterAgentTurnJournaled` it is the kind the action will intend (none for `Finish`).
    pub kind: Option<&'static str>,
    /// How many times this hook already passed this (point, kind) pair, starting at 0.
    pub occurrence: usize,
}

type Decide = dyn Fn(CrashPoint, &CrashCtx) -> bool + Send + Sync;

/// Cheap cloneable crash decision. Clones share their occurrence counters and the
/// "tripped" latch, so a hook handed both to the runner and to a `DurableExecutor` acts as
/// one: a crash inside the executor is seen by the runner as soon as the executor returns.
#[derive(Clone)]
pub struct CrashHook {
    decide: Arc<Decide>,
    seen: Arc<Mutex<HashMap<(CrashPoint, Option<&'static str>), usize>>>,
    tripped: Arc<Mutex<Option<CrashPoint>>>,
}

impl fmt::Debug for CrashHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashHook").field("tripped", &self.tripped()).finish_non_exhaustive()
    }
}

impl CrashHook {
    pub fn new(decide: impl Fn(CrashPoint, &CrashCtx) -> bool + Send + Sync + 'static) -> CrashHook {
        CrashHook { decide: Arc::new(decide), seen: Arc::default(), tripped: Arc::default() }
    }

    /// Fires the first time `point` is passed for an effect of kind `kind`.
    pub fn at(point: CrashPoint, kind: &'static str) -> CrashHook {
        CrashHook::new(move |p, ctx| p == point && ctx.kind == Some(kind) && ctx.occurrence == 0)
    }

    /// Records one pass of `point` and returns whether to crash there. Once it fires, the
    /// hook stays tripped.
    pub fn check(&self, point: CrashPoint, kind: Option<&'static str>) -> bool {
        let occurrence = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            let n = seen.entry((point, kind)).or_insert(0);
            *n += 1;
            *n - 1
        };
        if !(self.decide)(point, &CrashCtx { kind, occurrence }) {
            return false;
        }
        tracing::warn!(?point, ?kind, occurrence, "injected crash");
        *self.tripped.lock().unwrap_or_else(|e| e.into_inner()) = Some(point);
        true
    }

    /// The point this hook (or a clone of it) fired at, if it did.
    pub fn tripped(&self) -> Option<CrashPoint> {
        *self.tripped.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Options for `run_task_with` and `recover_with`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub crash: Option<CrashHook>,
}

impl RunOptions {
    pub fn crash_with(hook: CrashHook) -> RunOptions {
        RunOptions { crash: Some(hook) }
    }

    /// Whether to crash at `point` now.
    pub(crate) fn crashes_at(&self, point: CrashPoint, kind: Option<&'static str>) -> bool {
        self.crash.as_ref().is_some_and(|h| h.check(point, kind))
    }

    /// The point the hook already fired at elsewhere (inside the executor), if any.
    pub(crate) fn tripped(&self) -> Option<CrashPoint> {
        self.crash.as_ref().and_then(CrashHook::tripped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_counts_occurrences_per_point_and_kind_and_latches() {
        let hook = CrashHook::new(|p, ctx| p == CrashPoint::AfterDispatch && ctx.occurrence == 1);
        let clone = hook.clone();
        assert!(!hook.check(CrashPoint::AfterDispatch, Some("a")));
        assert!(!hook.check(CrashPoint::AfterDispatch, Some("b")), "counted per kind");
        assert!(!hook.check(CrashPoint::AfterIntent, Some("a")));
        assert_eq!(clone.tripped(), None);
        assert!(clone.check(CrashPoint::AfterDispatch, Some("a")), "clones share counters");
        assert_eq!(hook.tripped(), Some(CrashPoint::AfterDispatch));
    }

    #[test]
    fn at_fires_only_on_the_first_pass_of_its_kind() {
        let hook = CrashHook::at(CrashPoint::AfterComplete, "apply_patch");
        assert!(!hook.check(CrashPoint::AfterComplete, Some("read_snapshot")));
        assert!(!hook.check(CrashPoint::AfterComplete, None));
        assert!(hook.check(CrashPoint::AfterComplete, Some("apply_patch")));
        assert!(!hook.check(CrashPoint::AfterComplete, Some("apply_patch")));
    }

    #[test]
    fn default_options_never_crash() {
        let opts = RunOptions::default();
        for p in CrashPoint::ALL {
            assert!(!opts.crashes_at(p, Some("apply_patch")));
        }
        assert_eq!(opts.tripped(), None);
    }
}
