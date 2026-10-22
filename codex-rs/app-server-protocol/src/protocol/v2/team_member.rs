//! Operator-owned creation of a durable root for subsequent team enrollment.

use crate::JsonSchema;
use crate::TS;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMemberCreateParams {
    pub operation_id: String,
    pub coordinator_id: String,
    pub installation_id: String,
    /// An existing absolute directory on this worker, not the coordinator.
    pub cwd: String,
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub effort: Option<ReasoningEffort>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamMemberCreatePhase {
    Created,
    Failed,
    NeedsInspection,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMemberCreateResponse {
    pub operation_id: String,
    /// Reserved before startup; Failed does not imply that this thread exists.
    pub thread_id: String,
    pub phase: TeamMemberCreatePhase,
    pub error: Option<String>,
}
