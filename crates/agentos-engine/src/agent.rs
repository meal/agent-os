use std::collections::VecDeque;

use agentos_core::ids::Digest;

/// What the agent asks the runner to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    /// Apply a unified diff to the workspace.
    ApplyPatch(String),
    /// Run the protected verification profile against the current workspace.
    Verify,
    /// Stop; the task fails unless it already succeeded.
    Finish,
}

/// What the runner tells the agent after each step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Start { files: Vec<String>, workspace: Digest },
    PatchApplied { workspace: Digest },
    PatchRejected { reason: String },
    VersionConflict { expected: Digest, actual: Digest },
    Verification { passed: bool, summary: String },
    BudgetExhausted,
}

pub trait Agent {
    fn next(&mut self, obs: &Observation) -> AgentAction;
}

/// Deterministic agent that replays a script, then finishes forever.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    script: VecDeque<AgentAction>,
    seen: Vec<Observation>,
}

impl FakeAgent {
    pub fn scripted(actions: Vec<AgentAction>) -> FakeAgent {
        FakeAgent { script: actions.into(), seen: Vec::new() }
    }

    /// The scripted fixture solution: apply `patch`, verify, finish.
    pub fn from_fixture_patch(patch: String) -> FakeAgent {
        FakeAgent::scripted(vec![AgentAction::ApplyPatch(patch), AgentAction::Verify, AgentAction::Finish])
    }

    /// Every observation the agent was given, in order.
    pub fn observations(&self) -> &[Observation] {
        &self.seen
    }
}

impl Agent for FakeAgent {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        self.seen.push(obs.clone());
        self.script.pop_front().unwrap_or(AgentAction::Finish)
    }
}
