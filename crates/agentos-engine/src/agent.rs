use std::collections::VecDeque;

use agentos_core::contract::Contract;
use agentos_core::ids::Digest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

pub const SYSTEM_PROMPT: &str = "You are a coding agent working inside Agent OS. You act only through the tools. The repository is a snapshot: see files with list_files and read_file, change them only with apply_patch (a unified diff with a/ and b/ prefixes, paths relative to the repository root, only paths the contract allows), and prove the fix with run_verification, which runs a protected check you cannot see or change. The task succeeds only when run_verification passes on the final workspace. Make exactly one tool call per turn. Once the verification passes, call finish.";

const TRUNCATED_MARKER: &str = "\n[truncated at 65536 bytes]";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// The tool definitions sent with every request.
pub fn tools() -> Value {
    json!([
        tool("list_files", "List the files of the repository.", json!({}), &[]),
        tool(
            "read_file",
            "Read one file of the repository by its path relative to the repository root.",
            json!({"path": {"type": "string", "description": "path relative to the repository root"}}),
            &["path"],
        ),
        tool(
            "apply_patch",
            "Apply a unified diff (a/ and b/ prefixes, paths relative to the repository root) to the repository.",
            json!({"patch": {"type": "string", "description": "the unified diff"}}),
            &["patch"],
        ),
        tool("run_verification", "Run the protected verification against the current workspace.", json!({}), &[]),
        tool(
            "finish",
            "Finish the task once the verification passes.",
            json!({"summary": {"type": "string", "description": "a short summary of the change"}}),
            &["summary"],
        ),
    ])
}

/// An agent that drives the workspace through the Anthropic Messages API. The requests it
/// builds are a pure function of the observations it has seen: no clock, no local ids.
#[derive(Debug, Clone)]
pub struct ModelAgent {
    contract: Contract,
    model: String,
    history: Vec<Value>,
    pending_tool_use_id: Option<String>,
}

impl ModelAgent {
    pub fn new(contract: Contract, model: impl Into<String>) -> ModelAgent {
        ModelAgent { contract, model: model.into(), history: Vec::new(), pending_tool_use_id: None }
    }

    pub fn history(&self) -> &[Value] {
        &self.history
    }

    /// The serialized request for the current history. `serde_json` maps are sorted-key
    /// (`preserve_order` is off), so the bytes are canonical.
    pub fn request_body(&self) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": self.model,
            "max_tokens": self.contract.limits.max_output_tokens_per_request,
            "system": SYSTEM_PROMPT,
            "tools": tools(),
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "messages": self.history,
        }))
        .expect("a JSON value always serializes")
    }

    fn call(&mut self) -> AgentAction {
        let body = self.request_body();
        AgentAction::CallModel { request: Digest::of(&body), body }
    }

    fn push_result(&mut self, text: String, is_error: bool) {
        let Some(id) = self.pending_tool_use_id.clone() else { return };
        let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": text});
        if is_error {
            block["is_error"] = Value::Bool(true);
        }
        self.history.push(json!({"role": "user", "content": [block]}));
    }

    /// Answer the pending tool use with a result and ask the model again; without a
    /// pending call there is nothing to answer.
    fn answer(&mut self, text: String, is_error: bool) -> AgentAction {
        if self.pending_tool_use_id.is_none() {
            return AgentAction::Finish;
        }
        self.push_result(text, is_error);
        self.call()
    }

    fn on_response(&mut self, content: &Value) -> AgentAction {
        self.history.push(json!({"role": "assistant", "content": content}));
        self.pending_tool_use_id = None;
        let block = content
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")));
        let Some(block) = block else { return AgentAction::Finish };
        let Some(id) = block.get("id").and_then(Value::as_str) else { return AgentAction::Finish };
        self.pending_tool_use_id = Some(id.to_string());
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        let input = block.get("input");
        let string_field = |field: &str| input.and_then(|i| i.get(field)).and_then(Value::as_str).map(str::to_string);
        match name {
            "list_files" => AgentAction::ListFiles,
            "run_verification" => AgentAction::Verify,
            "finish" => AgentAction::Finish,
            "read_file" => match string_field("path") {
                Some(path) => AgentAction::ReadFile(path),
                None => self.answer("invalid tool input for read_file".into(), true),
            },
            "apply_patch" => match string_field("patch") {
                Some(patch) => AgentAction::ApplyPatch(patch),
                None => self.answer("invalid tool input for apply_patch".into(), true),
            },
            other => {
                let text = format!("unknown tool {}", bounded_name(other));
                self.answer(text, true)
            }
        }
    }
}

/// A model-chosen tool name is echoed back into JSON only, bounded in length.
fn bounded_name(name: &str) -> String {
    name.chars().take(64).collect()
}

impl Agent for ModelAgent {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        match obs {
            Observation::Start { files, .. } => {
                let text = format!(
                    "Goal: {}\n\nEditable paths: {}\n\nFiles in the repository:\n{}\n\nInspect the relevant files, fix the code with apply_patch, then run the verification.",
                    self.contract.goal,
                    self.contract.editable_paths.join(", "),
                    files.join("\n"),
                );
                self.history = vec![json!({"role": "user", "content": text})];
                self.pending_tool_use_id = None;
                self.call()
            }
            Observation::ModelResponse { content, .. } => self.on_response(content),
            Observation::Files { files } => self.answer(files.join("\n"), false),
            Observation::FileRead { content, truncated, .. } => {
                let text = if *truncated { format!("{content}{TRUNCATED_MARKER}") } else { content.clone() };
                self.answer(text, false)
            }
            Observation::FileReadRejected { reason } => self.answer(reason.clone(), true),
            Observation::PatchApplied { workspace } => {
                self.answer(format!("patch applied; the workspace is now {workspace}"), false)
            }
            Observation::PatchRejected { reason } => self.answer(format!("patch rejected: {reason}"), true),
            Observation::VersionConflict { expected, actual } => {
                self.answer(format!("version conflict: expected {expected}, actual {actual}"), true)
            }
            Observation::Verification { passed, summary } => {
                let verdict = if *passed { "passed" } else { "failed" };
                self.answer(format!("verification {verdict}: {summary}"), !*passed)
            }
            Observation::ModelCallFailed { .. } | Observation::ModelCallLost => self.call(),
            Observation::BudgetExhausted => AgentAction::Finish,
        }
    }
}
