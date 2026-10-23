//! Experimental team host protocol. The coordinator owns routing; hosts own execution.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

pub use codex_protocol::team_model::TeamModelSelection;
pub use codex_protocol::team_model::TeamSelection;
pub use codex_protocol::team_model::TeamSelectionOrigin;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamExecutionMode {
    Running,
    Paused,
    Stopped,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamControlState {
    pub coordinator_id: String,
    #[ts(type = "number")]
    pub revision: i64,
    pub mode: TeamExecutionMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAddress {
    pub machine_id: String,
    pub server_id: String,
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamEnrollment {
    pub address: TeamAddress,
    pub alias: String,
    pub manually_held: bool,
    /// A stopped turn has priority over subsequently queued messages.
    pub continuation_turn_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamHostReadParams {
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamHostReadResponse {
    pub installation_id: String,
    pub protocol_version: u32,
    pub control: Option<TeamControlState>,
    pub data: Vec<TeamEnrollment>,
    /// Runtime state of the listed roots and their descendants.
    pub runtime: Vec<TeamAgentRuntime>,
    #[serde(default)]
    pub retirements: Vec<super::TeamMemberRetirement>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentRuntime {
    #[serde(default)]
    pub activity: super::TeamActivity,
    #[serde(default)]
    pub environments: Vec<super::TeamExecutionEnvironment>,
    pub thread_id: String,
    pub root_thread_id: String,
    pub parent_thread_id: Option<String>,
    pub alias: String,
    pub loaded: bool,
    pub archived: bool,
    pub turn_id: Option<String>,
    pub current_model: Option<String>,
    pub current_effort: Option<String>,
    pub next_model: Option<String>,
    pub next_effort: Option<String>,
    pub model_error: Option<String>,
    #[serde(default)]
    pub model_origin: TeamSelectionOrigin,
    #[serde(default)]
    pub effort_origin: TeamSelectionOrigin,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub requested_selection_revision: Option<i64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub applied_selection_revision: Option<i64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub superseded_selection_revision: Option<i64>,
    pub in_flight: u32,
    pub reconciling: bool,
    pub manually_held: bool,
    pub continuation_turn_id: Option<String>,
    pub jobs: Vec<super::TeamTrackedJob>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamEnrollParams {
    #[ts(optional = nullable)]
    pub operation_id: Option<String>,
    pub address: TeamAddress,
    pub alias: String,
    pub control: TeamControlState,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamEnrollResponse {
    pub enrollment: TeamEnrollment,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamControlParams {
    pub control: TeamControlState,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamLeaveParams {
    #[ts(optional = nullable)]
    pub operation_id: Option<String>,
    pub coordinator_id: String,
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamLeaveResponse {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamControlResponse {
    pub control: TeamControlState,
    /// Acknowledges durable admission state, not completion of in-flight work.
    pub reconciled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamMessageKind {
    Request,
    Reply,
    Note,
    Board,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamDeliveryMode {
    #[default]
    NextTurn,
    SafeBoundary,
}

impl TeamDeliveryMode {
    /// Preserve the serialized identity of replies submitted before delivery was selectable.
    pub fn is_next_turn(&self) -> bool {
        matches!(self, Self::NextTurn)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMessage {
    pub id: String,
    pub sender: TeamAddress,
    pub recipient: Option<TeamAddress>,
    pub kind: TeamMessageKind,
    pub body: String,
    pub reply_to: Option<String>,
    pub topic: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamDelivery {
    pub id: String,
    pub thread_id: String,
    pub message: TeamMessage,
    pub mode: TeamDeliveryMode,
    pub wake: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamDeliverParams {
    pub coordinator_id: String,
    pub delivery: TeamDelivery,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamReceiptState {
    Pending,
    Rejected,
    Accepted,
    Completed,
    Interrupted,
    NeedsReconciliation,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamReceipt {
    pub delivery_id: String,
    pub thread_id: String,
    pub state: TeamReceiptState,
    pub turn_id: Option<String>,
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamDeliverResponse {
    pub receipt: TeamReceipt,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamChangedNotification {
    #[ts(type = "number")]
    pub revision: i64,
}
