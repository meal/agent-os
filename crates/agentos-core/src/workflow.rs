//! A workflow: a chain of task contracts run one after the other on one workspace. This
//! module is the pure part: parsing and validating the chain file, its digest, and the
//! chain's state derived from its steps' task states.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::{Contract, RepoRef};
use crate::ids::Digest;
use crate::state::TaskState;

pub const MIN_STEPS: usize = 2;
pub const MAX_STEPS: usize = 8;
/// The `repository.source` of every step after the first: the real input is the previous step's
/// verified result, supplied when the step starts.
pub const PREVIOUS_STEP_SOURCE: &str = "(previous step)";
const RECORDED: &str = "recorded-at-submission";

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid chain: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChainFile {
    name: String,
    repository: RepoRef,
    steps: Vec<StepFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    name: String,
    contract: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub contract: Contract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSpec {
    pub name: String,
    pub repository: RepoRef,
    pub steps: Vec<Step>,
}

fn check_name(what: &str, v: &str) -> Result<(), WorkflowError> {
    let ok = (1..=40).contains(&v.len())
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(WorkflowError::Invalid(format!(
            "{what} {v:?} must be 1..=40 characters of [a-z0-9-] and not start with '-'"
        )))
    }
}

impl ChainSpec {
    pub fn parse(json: &str) -> Result<ChainSpec, WorkflowError> {
        let file: ChainFile =
            serde_json::from_str(json).map_err(|e| WorkflowError::Invalid(e.to_string()))?;
        check_name("chain name", &file.name)?;
        if !(MIN_STEPS..=MAX_STEPS).contains(&file.steps.len()) {
            return Err(WorkflowError::Invalid(format!(
                "a chain has {MIN_STEPS} to {MAX_STEPS} steps, got {}",
                file.steps.len()
            )));
        }
        let mut steps: Vec<Step> = Vec::new();
        for (i, s) in file.steps.into_iter().enumerate() {
            check_name("step name", &s.name)?;
            if steps.iter().any(|p| p.name == s.name) {
                return Err(WorkflowError::Invalid(format!(
                    "duplicate step name {:?}",
                    s.name
                )));
            }
            let mut value = s.contract;
            let obj = value.as_object_mut().ok_or_else(|| {
                WorkflowError::Invalid(format!("step {:?}: contract must be an object", s.name))
            })?;
            if i == 0 && obj.contains_key("repository") {
                return Err(WorkflowError::Invalid(format!(
                    "step {:?}: the first step starts from the chain's top-level repository; remove its own",
                    s.name
                )));
            }
            if i > 0 && obj.contains_key("repository") {
                return Err(WorkflowError::Invalid(format!(
                    "step {:?}: only the first step sets a repository; this one starts from the previous step's result",
                    s.name
                )));
            }
            let repo = if i == 0 {
                serde_json::to_value(&file.repository).expect("serializes")
            } else {
                serde_json::json!({ "source": PREVIOUS_STEP_SOURCE, "revision": RECORDED })
            };
            obj.insert("repository".into(), repo);
            let contract = Contract::parse(&value.to_string())
                .map_err(|e| WorkflowError::Invalid(format!("step {:?}: {e}", s.name)))?;
            steps.push(Step {
                name: s.name,
                contract,
            });
        }
        Ok(ChainSpec {
            name: file.name,
            repository: file.repository,
            steps,
        })
    }

    /// What the owner approves: the digest of the parsed chain, every step's contract included.
    pub fn digest(&self) -> Digest {
        Digest::of(&serde_json::to_vec(self).expect("a chain serializes"))
    }
}

/// What the chain knows about one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// No task yet.
    Pending,
    Task(TaskState),
    /// Never started because the chain was cancelled.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowState {
    Running,
    Succeeded,
    Failed { step: usize },
    Cancelled { step: usize },
}

impl WorkflowState {
    pub fn label(&self) -> &'static str {
        match self {
            WorkflowState::Running => "RUNNING",
            WorkflowState::Succeeded => "SUCCEEDED",
            WorkflowState::Failed { .. } => "FAILED",
            WorkflowState::Cancelled { .. } => "CANCELLED",
        }
    }
}

/// The first failed step fails the chain, then the first cancelled or skipped one cancels it;
/// all steps succeeded is success; anything else is still running.
pub fn derive_state(steps: &[StepStatus]) -> WorkflowState {
    let first = |want: &dyn Fn(&StepStatus) -> bool| steps.iter().position(want);
    if let Some(step) = first(&|s| *s == StepStatus::Task(TaskState::Failed)) {
        return WorkflowState::Failed { step };
    }
    if let Some(step) = first(&|s| {
        matches!(
            s,
            StepStatus::Task(TaskState::Cancelled) | StepStatus::Skipped
        )
    }) {
        return WorkflowState::Cancelled { step };
    }
    if !steps.is_empty()
        && steps
            .iter()
            .all(|s| *s == StepStatus::Task(TaskState::Succeeded))
    {
        return WorkflowState::Succeeded;
    }
    WorkflowState::Running
}

/// The chain's state. `failed` is a failure the store recorded without any step showing it
/// (`fail_workflow`: a handoff could not be built); it fails the chain at its first skipped
/// step, or at the last step if none is skipped.
pub fn state_of_steps(steps: &[StepStatus], failed: bool) -> WorkflowState {
    if failed {
        let step = steps
            .iter()
            .position(|s| *s == StepStatus::Skipped)
            .unwrap_or(steps.len().saturating_sub(1));
        return WorkflowState::Failed { step };
    }
    derive_state(steps)
}

/// `READY` for a chain that is not approved yet, else the state's own label.
pub fn display_label(state: WorkflowState, approved: bool) -> &'static str {
    if state == WorkflowState::Running && !approved {
        "READY"
    } else {
        state.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn contract(goal: &str) -> Value {
        json!({
            "goal": goal, "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        })
    }

    fn chain(steps: Vec<(&str, Value)>) -> String {
        json!({
            "name": "fix-and-review",
            "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
            "steps": steps.into_iter().map(|(n, c)| json!({ "name": n, "contract": c })).collect::<Vec<_>>()
        })
        .to_string()
    }

    #[test]
    fn a_two_step_chain_parses_and_later_steps_point_at_the_previous_one() {
        let spec = ChainSpec::parse(&chain(vec![
            ("implement", contract("fix it")),
            ("review", contract("review it")),
        ]))
        .unwrap();
        assert_eq!(spec.steps.len(), 2);
        assert_eq!(spec.steps[0].contract.repository.source, "/work/repo");
        assert_eq!(
            spec.steps[1].contract.repository.source,
            PREVIOUS_STEP_SOURCE
        );
        assert_eq!(
            spec.digest(),
            ChainSpec::parse(&chain(vec![
                ("implement", contract("fix it")),
                ("review", contract("review it")),
            ]))
            .unwrap()
            .digest()
        );
    }

    #[test]
    fn the_digest_changes_with_any_step() {
        let a = ChainSpec::parse(&chain(vec![("a", contract("x")), ("b", contract("y"))])).unwrap();
        let b = ChainSpec::parse(&chain(vec![("a", contract("x")), ("b", contract("z"))])).unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn invalid_chains_are_refused() {
        let ok = |n| (n, contract("g"));
        let mut with_repo = contract("g");
        with_repo["repository"] = json!({ "source": "/x", "revision": "recorded-at-submission" });
        let nine: Vec<(String, Value)> = (0..9).map(|i| (format!("s{i}"), contract("g"))).collect();
        let nine: Vec<(&str, Value)> = nine.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
        for (what, text) in [
            ("one step", chain(vec![ok("a")])),
            ("nine steps", chain(nine)),
            ("duplicate names", chain(vec![ok("a"), ok("a")])),
            (
                "bad name",
                chain(vec![ok("a"), ("Not Plain", contract("g"))]),
            ),
            ("leading dash", chain(vec![ok("a"), ("-b", contract("g"))])),
            (
                "repository on step 2",
                chain(vec![ok("a"), ("b", with_repo)]),
            ),
            ("not json", "nope".to_string()),
        ] {
            assert!(ChainSpec::parse(&text).is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn a_first_step_with_its_own_repository_is_refused() {
        let mut with_repo = contract("g");
        with_repo["repository"] = json!({ "source": "/x", "revision": "recorded-at-submission" });
        let e = ChainSpec::parse(&chain(vec![("a", with_repo), ("b", contract("g"))])).unwrap_err();
        assert!(
            e.to_string().contains("the chain's top-level repository"),
            "{e}"
        );
    }

    #[test]
    fn an_invalid_step_contract_names_the_step() {
        let mut bad = contract("g");
        bad["limits"]["tool_actions"] = json!(0);
        let e = ChainSpec::parse(&chain(vec![("a", contract("g")), ("b", bad)])).unwrap_err();
        assert!(e.to_string().contains("step \"b\""), "{e}");
    }

    #[test]
    fn state_is_derived_from_the_steps() {
        use crate::state::TaskState as T;
        use StepStatus::*;
        assert_eq!(derive_state(&[Pending, Pending]), WorkflowState::Running);
        assert_eq!(
            derive_state(&[Task(T::Running), Pending]),
            WorkflowState::Running
        );
        assert_eq!(
            derive_state(&[Task(T::Succeeded), Pending]),
            WorkflowState::Running
        );
        assert_eq!(
            derive_state(&[Task(T::Succeeded), Task(T::Succeeded)]),
            WorkflowState::Succeeded
        );
        assert_eq!(
            derive_state(&[Task(T::Failed), Pending]),
            WorkflowState::Failed { step: 0 }
        );
        assert_eq!(
            derive_state(&[Task(T::Succeeded), Task(T::Cancelled)]),
            WorkflowState::Cancelled { step: 1 }
        );
        // A cancel between steps skips the rest without any cancelled task.
        assert_eq!(
            derive_state(&[Task(T::Succeeded), Skipped]),
            WorkflowState::Cancelled { step: 1 }
        );
        assert_eq!(WorkflowState::Failed { step: 0 }.label(), "FAILED");
    }

    #[test]
    fn a_recorded_failure_fails_the_chain_at_its_first_skipped_step() {
        let steps = [
            StepStatus::Task(TaskState::Succeeded),
            StepStatus::Skipped,
            StepStatus::Skipped,
        ];
        assert_eq!(
            state_of_steps(&steps, true),
            WorkflowState::Failed { step: 1 }
        );
    }

    #[test]
    fn a_recorded_failure_without_a_skipped_step_fails_the_last_one() {
        let steps = [StepStatus::Task(TaskState::Succeeded), StepStatus::Pending];
        assert_eq!(
            state_of_steps(&steps, true),
            WorkflowState::Failed { step: 1 }
        );
    }

    #[test]
    fn without_a_recorded_failure_the_state_is_derived() {
        let steps = [StepStatus::Task(TaskState::Succeeded), StepStatus::Pending];
        assert_eq!(state_of_steps(&steps, false), WorkflowState::Running);
    }

    #[test]
    fn a_running_chain_nobody_approved_is_ready() {
        assert_eq!(display_label(WorkflowState::Running, false), "READY");
        assert_eq!(
            display_label(WorkflowState::Running, true),
            WorkflowState::Running.label()
        );
        assert_eq!(
            display_label(WorkflowState::Succeeded, false),
            WorkflowState::Succeeded.label()
        );
    }
}
