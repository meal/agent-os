use agentos_core::contract::{Capability, Contract};
use agentos_core::effect::{AttemptId, EffectId, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::job::JobState;
use agentos_store::effects::UsageSummary;
use agentos_store::read::{TaskCursor, TaskListRow};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct GuestImageRef {
    pub id: String,
    pub digest: Digest,
}
#[derive(Debug, Serialize)]
pub(crate) struct OutstandingEffect {
    pub effect_id: EffectId,
    pub kind: String,
    pub state: EffectState,
    pub lease_generation: u64,
}
#[derive(Debug, Serialize)]
pub(crate) struct JobSummary {
    pub effect_id: EffectId,
    pub attempt_id: AttemptId,
    pub lease_generation: u64,
    pub state: Option<JobState>,
    pub alive: bool,
    pub receipt: bool,
}
#[derive(Debug, Serialize)]
pub(crate) struct CapabilitySummary {
    pub operation: Capability,
    pub handle_prefix: String,
    pub revoked: bool,
    pub expires_ts: Option<i64>,
}
#[derive(Debug, Serialize)]
pub(crate) struct StatusView {
    pub task_id: TaskId,
    pub state: String,
    pub step: u32,
    pub cancel_requested: bool,
    pub workspace_digest: Digest,
    pub verified_digest: Option<Digest>,
    pub actions_used: u32,
    pub usage: UsageSummary,
    pub outstanding_effects: Vec<OutstandingEffect>,
    pub jobs: Vec<JobSummary>,
    pub capabilities: Vec<CapabilitySummary>,
    pub worker: String,
    pub model: String,
    pub model_policy_version: u32,
    pub model_limits_version: u32,
    pub model_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_image: Option<GuestImageRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jailed: Option<bool>,
}
#[derive(Debug, Serialize)]
pub(crate) struct TaskSummary {
    pub row: TaskListRow,
    pub worker: String,
    pub model: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct TaskListView {
    pub rows: Vec<TaskSummary>,
    pub next: Option<TaskCursor>,
}
#[derive(Debug, Serialize)]
pub(crate) struct TaskDetail {
    pub status: StatusView,
    pub contract: Contract,
    pub contract_digest: Digest,
    pub repository_digest: Option<Digest>,
    pub profile_digest: Option<Digest>,
}
impl OutstandingEffect {
    pub(crate) fn state_label(&self) -> &'static str {
        match self.state {
            EffectState::Intended => "INTENDED",
            EffectState::Dispatched => "DISPATCHED",
            EffectState::Completed => "COMPLETED",
            EffectState::Failed => "FAILED",
            EffectState::Unknown => "UNKNOWN",
            EffectState::Abandoned => "ABANDONED",
        }
    }
}
