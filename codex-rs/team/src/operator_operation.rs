//! Durable operator work remains observable after a client disconnects.

use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamBoardPurgeOperation;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberCreateRequest {
    pub machine_id: String,
    pub server_id: String,
    pub alias: String,
    pub cwd: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamMemberCreateState {
    Creating,
    EnrollmentPending,
    Created,
    Failed,
    NeedsInspection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberCreateOperation {
    pub operation_id: String,
    pub request: TeamMemberCreateRequest,
    pub phase: TeamMemberCreateState,
    pub address: Option<TeamAddress>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "operation", rename_all = "camelCase")]
pub enum TeamOperatorOperation {
    MemberCreate(TeamMemberCreateOperation),
    BoardPurge(TeamBoardPurgeOperation),
}
