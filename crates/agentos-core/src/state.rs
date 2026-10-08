use serde::{Deserialize, Serialize};

use crate::contract::Limits;
use crate::ids::{Digest, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    Ready,
    Running,
    Waiting,
    Paused,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// The upper-case name shown to owners (CLI output, export manifests).
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Ready => "READY",
            TaskState::Running => "RUNNING",
            TaskState::Waiting => "WAITING",
            TaskState::Paused => "PAUSED",
            TaskState::Verifying => "VERIFYING",
            TaskState::Succeeded => "SUCCEEDED",
            TaskState::Failed => "FAILED",
            TaskState::Cancelled => "CANCELLED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub cancel_requested: bool,
    pub workspace_digest: Digest,
    pub verified_digest: Option<Digest>,
    pub actions_used: u32,
    pub step: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskEvent {
    Started,
    Waiting,
    Woken,
    Paused,
    Resumed,
    VerifyStarted,
    VerifyPassed { digest: Digest },
    VerifyFailed,
    WorkspaceUpdated { digest: Digest },
    ActionUsed,
    CancelRequested,
    CancelCompleted,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("task is in terminal state {0:?}")]
    Terminal(TaskState),
    #[error("event {event} is not valid in state {state:?}")]
    InvalidTransition {
        state: TaskState,
        event: &'static str,
    },
    #[error("event {event} rejected: cancellation requested")]
    CancelRequested { event: &'static str },
    #[error("cancel completed without a cancel request")]
    CancelNotRequested,
    #[error("verification digest {got} does not match workspace digest {expected}")]
    DigestMismatch { expected: Digest, got: Digest },
    #[error("tool action limit of {0} exhausted")]
    ActionLimit(u32),
}

impl TaskEvent {
    fn name(&self) -> &'static str {
        match self {
            TaskEvent::Started => "Started",
            TaskEvent::Waiting => "Waiting",
            TaskEvent::Woken => "Woken",
            TaskEvent::Paused => "Paused",
            TaskEvent::Resumed => "Resumed",
            TaskEvent::VerifyStarted => "VerifyStarted",
            TaskEvent::VerifyPassed { .. } => "VerifyPassed",
            TaskEvent::VerifyFailed => "VerifyFailed",
            TaskEvent::WorkspaceUpdated { .. } => "WorkspaceUpdated",
            TaskEvent::ActionUsed => "ActionUsed",
            TaskEvent::CancelRequested => "CancelRequested",
            TaskEvent::CancelCompleted => "CancelCompleted",
            TaskEvent::Failed { .. } => "Failed",
        }
    }

    /// Once cancellation is requested only these events are accepted; cancel always wins.
    fn allowed_during_cancel(&self) -> bool {
        matches!(
            self,
            TaskEvent::CancelRequested | TaskEvent::CancelCompleted | TaskEvent::Failed { .. }
        )
    }
}

impl Task {
    pub fn new(id: TaskId, base_workspace_digest: Digest) -> Task {
        Task {
            id,
            state: TaskState::Ready,
            cancel_requested: false,
            workspace_digest: base_workspace_digest,
            verified_digest: None,
            actions_used: 0,
            step: 0,
        }
    }

    pub fn may_dispatch(&self) -> bool {
        !self.cancel_requested && matches!(self.state, TaskState::Running | TaskState::Verifying)
    }
}

pub fn reduce(task: &Task, ev: &TaskEvent, limits: &Limits) -> Result<Task, TransitionError> {
    use TaskState::*;

    if task.state.is_terminal() {
        return Err(TransitionError::Terminal(task.state));
    }
    if task.cancel_requested && !ev.allowed_during_cancel() {
        return Err(TransitionError::CancelRequested { event: ev.name() });
    }
    let invalid = || TransitionError::InvalidTransition {
        state: task.state,
        event: ev.name(),
    };

    let mut next = task.clone();
    match (task.state, ev) {
        (_, TaskEvent::CancelRequested) => next.cancel_requested = true,
        (_, TaskEvent::CancelCompleted) => {
            if !task.cancel_requested {
                return Err(TransitionError::CancelNotRequested);
            }
            next.state = Cancelled;
        }
        (_, TaskEvent::Failed { .. }) => next.state = Failed,
        (Ready, TaskEvent::Started) => next.state = Running,
        (Running, TaskEvent::Waiting) => next.state = Waiting,
        (Waiting, TaskEvent::Woken) => next.state = Running,
        (Running | Waiting, TaskEvent::Paused) => next.state = Paused,
        (Paused, TaskEvent::Resumed) => next.state = Running,
        (Running, TaskEvent::VerifyStarted) => next.state = Verifying,
        (Verifying, TaskEvent::VerifyPassed { digest }) => {
            if *digest != task.workspace_digest {
                return Err(TransitionError::DigestMismatch {
                    expected: task.workspace_digest,
                    got: *digest,
                });
            }
            next.verified_digest = Some(*digest);
            next.state = Succeeded;
        }
        (Verifying, TaskEvent::VerifyFailed) => {
            next.state = if task.actions_used < limits.tool_actions {
                Running
            } else {
                Failed
            };
        }
        // Also accepted while Paused: an effect that was in flight when the pause landed
        // still changed the workspace, and the digest must stay true for the resume.
        (Running | Paused, TaskEvent::WorkspaceUpdated { digest }) => {
            next.workspace_digest = *digest;
            next.verified_digest = None;
        }
        (Running, TaskEvent::ActionUsed) => {
            if task.actions_used >= limits.tool_actions {
                return Err(TransitionError::ActionLimit(limits.tool_actions));
            }
            next.actions_used = task.actions_used.saturating_add(1);
        }
        _ => return Err(invalid()),
    }
    next.step = task.step.saturating_add(1);
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskEvent::*;

    fn limits() -> Limits {
        Limits {
            model_requests: 10,
            max_output_tokens_per_request: 1000,
            tool_actions: 3,
            deadline_seconds: 60,
            worker_vcpus: 1,
            worker_memory_mib: 256,
            worker_disk_mib: None,
            worker_scratch_mib: None,
            worker_disk_bandwidth_mib_s: None,
            worker_disk_iops: None,
        }
    }
    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }
    fn fresh() -> Task {
        Task::new(TaskId::new(), d("base"))
    }
    fn at(state: TaskState) -> Task {
        Task { state, ..fresh() }
    }
    fn running() -> Task {
        at(TaskState::Running)
    }
    fn all_events() -> Vec<TaskEvent> {
        vec![
            Started,
            Waiting,
            Woken,
            Paused,
            Resumed,
            VerifyStarted,
            VerifyPassed { digest: d("base") },
            VerifyFailed,
            WorkspaceUpdated { digest: d("x") },
            ActionUsed,
            CancelRequested,
            CancelCompleted,
            Failed { reason: "r".into() },
        ]
    }

    #[test]
    fn labels_are_upper_case_variant_names() {
        for s in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::Waiting,
            TaskState::Paused,
            TaskState::Verifying,
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            assert_eq!(s.label(), format!("{s:?}").to_uppercase());
        }
    }

    #[test]
    fn new_task_starts_ready() {
        let t = fresh();
        assert_eq!(t.state, TaskState::Ready);
        assert!(!t.cancel_requested);
        assert_eq!(t.verified_digest, None);
        assert_eq!((t.actions_used, t.step), (0, 0));
        assert_eq!(t.workspace_digest, d("base"));
        assert!(!t.may_dispatch());
    }

    #[test]
    fn happy_path_to_succeeded() {
        let l = limits();
        let t = reduce(&fresh(), &Started, &l).unwrap();
        let t = reduce(&t, &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        let t = reduce(&t, &VerifyPassed { digest: d("w1") }, &l).unwrap();
        assert_eq!(t.state, TaskState::Succeeded);
        assert_eq!(t.verified_digest, Some(d("w1")));
        assert_eq!(t.step, 4);
    }

    #[test]
    fn reduce_is_pure() {
        let t = running();
        let before = t.clone();
        let n = reduce(&t, &ActionUsed, &limits()).unwrap();
        assert_eq!(t, before);
        assert_eq!(n.actions_used, 1);
    }

    #[test]
    fn success_requires_evidence_for_final_revision() {
        let l = limits();
        let t = reduce(&running(), &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        let err = reduce(&t, &VerifyPassed { digest: d("w0") }, &l).unwrap_err();
        assert!(matches!(err, TransitionError::DigestMismatch { .. }));
    }

    #[test]
    fn workspace_update_after_verify_cycle_requires_new_evidence() {
        let l = limits();
        let t = reduce(&running(), &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        let t = reduce(&t, &VerifyFailed, &l).unwrap();
        let t = reduce(&t, &WorkspaceUpdated { digest: d("w2") }, &l).unwrap();
        assert_eq!(t.verified_digest, None);
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        assert!(reduce(&t, &VerifyPassed { digest: d("w1") }, &l).is_err());
        assert!(reduce(&t, &VerifyPassed { digest: d("w2") }, &l).is_ok());
    }

    #[test]
    fn workspace_updated_only_in_running_or_paused() {
        let l = limits();
        for s in [TaskState::Ready, TaskState::Waiting, TaskState::Verifying] {
            assert!(
                reduce(&at(s), &WorkspaceUpdated { digest: d("w") }, &l).is_err(),
                "{s:?}"
            );
        }
        for s in [TaskState::Running, TaskState::Paused] {
            let t = Task {
                verified_digest: Some(d("base")),
                ..at(s)
            };
            let n = reduce(&t, &WorkspaceUpdated { digest: d("w") }, &l).unwrap();
            assert_eq!(n.state, s, "the update never changes the lifecycle state");
            assert_eq!(n.workspace_digest, d("w"));
            assert_eq!(n.verified_digest, None);
            assert_eq!(n.step, t.step + 1);
        }
    }

    #[test]
    fn effect_finishing_while_paused_keeps_the_digest_current_for_resume() {
        let l = limits();
        let t = reduce(&running(), &Paused, &l).unwrap();
        let t = reduce(&t, &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
        let t = reduce(&t, &Resumed, &l).unwrap();
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        assert_eq!(
            reduce(&t, &VerifyPassed { digest: d("w1") }, &l)
                .unwrap()
                .state,
            TaskState::Succeeded
        );
    }

    #[test]
    fn workspace_updated_rejected_while_cancel_pending_even_when_paused() {
        let l = limits();
        let t = Task {
            cancel_requested: true,
            ..at(TaskState::Paused)
        };
        assert!(matches!(
            reduce(&t, &WorkspaceUpdated { digest: d("w") }, &l),
            Err(TransitionError::CancelRequested { .. })
        ));
    }

    /// Which (state, event) pairs the reducer accepts, with no cancel pending.
    #[test]
    fn valid_from_state_table() {
        use TaskState as S;
        let l = limits();
        let table: [(&str, &[S]); 13] = [
            ("Started", &[S::Ready]),
            ("Waiting", &[S::Running]),
            ("Woken", &[S::Waiting]),
            ("Paused", &[S::Running, S::Waiting]),
            ("Resumed", &[S::Paused]),
            ("VerifyStarted", &[S::Running]),
            ("VerifyPassed", &[S::Verifying]),
            ("VerifyFailed", &[S::Verifying]),
            ("WorkspaceUpdated", &[S::Running, S::Paused]),
            ("ActionUsed", &[S::Running]),
            (
                "CancelRequested",
                &[S::Ready, S::Running, S::Waiting, S::Paused, S::Verifying],
            ),
            ("CancelCompleted", &[]),
            (
                "Failed",
                &[S::Ready, S::Running, S::Waiting, S::Paused, S::Verifying],
            ),
        ];
        // all_events() uses the base digest for VerifyPassed, so it is acceptable from Verifying.
        for ev in all_events() {
            let (_, valid) = table.iter().find(|(n, _)| *n == ev.name()).unwrap();
            for s in [S::Ready, S::Running, S::Waiting, S::Paused, S::Verifying] {
                assert_eq!(
                    reduce(&at(s), &ev, &l).is_ok(),
                    valid.contains(&s),
                    "{s:?} {ev:?}"
                );
            }
        }
    }

    #[test]
    fn failed_verification_returns_to_running_within_limits() {
        let t = Task {
            actions_used: 2,
            ..at(TaskState::Verifying)
        };
        let t = reduce(&t, &VerifyFailed, &limits()).unwrap();
        assert_eq!(t.state, TaskState::Running);
    }

    #[test]
    fn failed_verification_with_exhausted_limits_goes_failed() {
        let t = Task {
            actions_used: 3,
            ..at(TaskState::Verifying)
        };
        let t = reduce(&t, &VerifyFailed, &limits()).unwrap();
        assert_eq!(t.state, TaskState::Failed);
    }

    #[test]
    fn verify_failed_only_from_verifying() {
        assert!(reduce(&running(), &VerifyFailed, &limits()).is_err());
    }

    #[test]
    fn action_used_increments_and_errors_past_limit() {
        let l = limits();
        let mut t = running();
        for i in 1..=3 {
            t = reduce(&t, &ActionUsed, &l).unwrap();
            assert_eq!(t.actions_used, i);
        }
        assert_eq!(
            reduce(&t, &ActionUsed, &l),
            Err(TransitionError::ActionLimit(3))
        );
    }

    #[test]
    fn pause_and_resume() {
        let l = limits();
        let p = reduce(&running(), &Paused, &l).unwrap();
        assert_eq!(p.state, TaskState::Paused);
        assert_eq!(reduce(&p, &Resumed, &l).unwrap().state, TaskState::Running);
        let w = reduce(&running(), &Waiting, &l).unwrap();
        assert_eq!(w.state, TaskState::Waiting);
        assert_eq!(reduce(&w, &Paused, &l).unwrap().state, TaskState::Paused);
        assert_eq!(reduce(&w, &Woken, &l).unwrap().state, TaskState::Running);
        assert!(reduce(&fresh(), &Paused, &l).is_err());
        assert!(reduce(&running(), &Resumed, &l).is_err());
    }

    #[test]
    fn cancel_request_blocks_dispatch_but_state_not_cancelled_until_completed() {
        let l = limits();
        let t = reduce(&running(), &CancelRequested, &l).unwrap();
        assert!(t.cancel_requested);
        assert!(!t.may_dispatch());
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(
            reduce(&t, &CancelCompleted, &l).unwrap().state,
            TaskState::Cancelled
        );
    }

    #[test]
    fn cancel_requested_accepts_only_cancel_and_failure_events() {
        let l = limits();
        for s in [TaskState::Running, TaskState::Verifying] {
            let t = Task {
                cancel_requested: true,
                ..at(s)
            };
            let mut events = all_events();
            events.push(VerifyPassed {
                digest: t.workspace_digest,
            });
            for ev in events {
                let allowed = matches!(ev, CancelRequested | CancelCompleted | Failed { .. });
                let res = reduce(&t, &ev, &l);
                assert_eq!(res.is_ok(), allowed, "{s:?} {ev:?}");
                if !allowed {
                    assert!(
                        matches!(res, Err(TransitionError::CancelRequested { .. })),
                        "{s:?} {ev:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pending_cancel_rejects_matching_verify_passed() {
        let l = limits();
        let t = reduce(&running(), &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
        let t = reduce(&t, &VerifyStarted, &l).unwrap();
        let t = reduce(&t, &CancelRequested, &l).unwrap();
        assert!(matches!(
            reduce(&t, &VerifyPassed { digest: d("w1") }, &l),
            Err(TransitionError::CancelRequested { .. })
        ));
        assert!(reduce(&t, &VerifyFailed, &l).is_err());
    }

    #[test]
    fn repeated_cancel_request_is_idempotent_and_counts_as_a_step() {
        let l = limits();
        let t = reduce(&running(), &CancelRequested, &l).unwrap();
        let t2 = reduce(&t, &CancelRequested, &l).unwrap();
        assert!(t2.cancel_requested);
        assert_eq!(t2.state, TaskState::Running);
        assert_eq!(t2.step, t.step + 1);
    }

    #[test]
    fn failed_still_allowed_while_cancel_pending() {
        let t = Task {
            cancel_requested: true,
            ..running()
        };
        let f = reduce(&t, &Failed { reason: "x".into() }, &limits()).unwrap();
        assert_eq!(f.state, TaskState::Failed);
    }

    #[test]
    fn cancel_requested_is_allowed_in_every_non_terminal_state() {
        for s in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::Waiting,
            TaskState::Paused,
            TaskState::Verifying,
        ] {
            let t = reduce(&at(s), &CancelRequested, &limits()).unwrap();
            assert!(t.cancel_requested);
            assert_eq!(t.state, s);
        }
    }

    #[test]
    fn cancel_completed_without_request_is_rejected() {
        assert_eq!(
            reduce(&running(), &CancelCompleted, &limits()),
            Err(TransitionError::CancelNotRequested)
        );
    }

    #[test]
    fn failed_valid_from_any_non_terminal_state() {
        for s in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::Waiting,
            TaskState::Paused,
            TaskState::Verifying,
        ] {
            let t = reduce(
                &at(s),
                &Failed {
                    reason: "boom".into(),
                },
                &limits(),
            )
            .unwrap();
            assert_eq!(t.state, TaskState::Failed);
        }
    }

    #[test]
    fn terminal_states_reject_every_event() {
        for s in [
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            for cancel in [false, true] {
                let t = Task {
                    cancel_requested: cancel,
                    ..at(s)
                };
                for ev in all_events() {
                    assert_eq!(
                        reduce(&t, &ev, &limits()),
                        Err(TransitionError::Terminal(s)),
                        "{s:?} {ev:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn step_increments_on_accepted_events_only() {
        let l = limits();
        let t = reduce(&fresh(), &Started, &l).unwrap();
        assert_eq!(t.step, 1);
        assert!(reduce(&t, &Started, &l).is_err());
        let t2 = reduce(&t, &CancelRequested, &l).unwrap();
        assert_eq!(t2.step, 2);
    }

    #[test]
    fn step_and_actions_saturate_instead_of_panicking() {
        let t = Task {
            step: u32::MAX,
            ..running()
        };
        assert_eq!(reduce(&t, &ActionUsed, &limits()).unwrap().step, u32::MAX);
    }
}
