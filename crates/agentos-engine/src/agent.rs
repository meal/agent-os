use std::collections::VecDeque;

use agentos_core::ids::Digest;
use serde::{Deserialize, Serialize};

/// What the agent asks the runner to do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentAction {
    /// Apply a unified diff to the workspace.
    ApplyPatch(String),
    /// Run the protected verification profile against the current workspace.
    Verify,
    /// Ask the model for the next move. `request` is the digest of `body`, the serialized
    /// request; the body is not journaled in the turn (it lives in a blob), so a journaled
    /// action has an empty one and turns are compared by digest.
    CallModel {
        request: Digest,
        #[serde(skip)]
        body: Vec<u8>,
    },
    /// List the files of the workspace.
    ListFiles,
    /// Read one file of the workspace.
    ReadFile(String),
    /// Stop; the task fails unless it already succeeded.
    Finish,
}

/// What the runner tells the agent after each step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Observation {
    Start { files: Vec<String>, workspace: Digest },
    PatchApplied { workspace: Digest },
    PatchRejected { reason: String },
    VersionConflict { expected: Digest, actual: Digest },
    Verification { passed: bool, summary: String },
    BudgetExhausted,
    /// The model answered: its content blocks, why it stopped and the tokens it produced.
    ModelResponse { content: serde_json::Value, stop_reason: String, output_tokens: u64 },
    /// The model request failed with an answer (an HTTP error, a refusal).
    ModelCallFailed { reason: String },
    /// The model request was sent but its outcome was lost; it was forfeited and counts as
    /// used.
    ModelCallLost,
    /// The workspace's files.
    Files { files: Vec<String> },
    FileRead { path: String, content: String, truncated: bool },
    FileReadRejected { reason: String },
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
