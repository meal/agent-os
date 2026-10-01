use agentos_core::contract::Limits;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::{reduce, Task, TaskEvent, TaskState};
use proptest::prelude::*;

fn digest() -> impl Strategy<Value = Digest> {
    (0u8..3).prop_map(|n| Digest::of(&[n]))
}

fn event() -> impl Strategy<Value = TaskEvent> {
    prop_oneof![
        Just(TaskEvent::Started),
        Just(TaskEvent::Waiting),
        Just(TaskEvent::Woken),
        Just(TaskEvent::Paused),
        Just(TaskEvent::Resumed),
        Just(TaskEvent::VerifyStarted),
        digest().prop_map(|digest| TaskEvent::VerifyPassed { digest }),
        Just(TaskEvent::VerifyFailed),
        digest().prop_map(|digest| TaskEvent::WorkspaceUpdated { digest }),
        Just(TaskEvent::ActionUsed),
        Just(TaskEvent::CancelRequested),
        Just(TaskEvent::CancelCompleted),
        Just(TaskEvent::Failed { reason: "r".into() }),
    ]
}

fn limits() -> Limits {
    Limits {
        model_requests: 10,
        max_output_tokens_per_request: 1000,
        tool_actions: 4,
        deadline_seconds: 60,
        worker_vcpus: 1,
        worker_memory_mib: 256,
    }
}

proptest! {
    #[test]
    fn random_sequences_preserve_invariants(events in prop::collection::vec(event(), 0..80)) {
        let l = limits();
        let mut task = Task::new(TaskId::new(), Digest::of(&[0]));
        for ev in events {
            let before = task.clone();
            match reduce(&task, &ev, &l) {
                Ok(next) => {
                    prop_assert!(!before.state.is_terminal());
                    prop_assert_eq!(next.step, before.step + 1);
                    prop_assert!(next.actions_used <= l.tool_actions);
                    if before.cancel_requested {
                        prop_assert!(next.cancel_requested);
                    }
                    task = next;
                }
                Err(_) => {}
            }
            if before.state.is_terminal() {
                prop_assert_eq!(&task, &before);
            }
            if task.cancel_requested {
                prop_assert_ne!(task.state, TaskState::Succeeded);
            }
            if task.state == TaskState::Succeeded {
                prop_assert_eq!(task.verified_digest, Some(task.workspace_digest));
            }
        }
    }
}
