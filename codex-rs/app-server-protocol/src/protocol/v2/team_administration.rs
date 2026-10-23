//! Enrolled teammates manage collaboration without changing execution policy.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::TeamAddress;
use super::TeamAgentTarget;
use super::TeamBoardPurgeScope;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
pub enum TeamMemberActionParams {
    Create {
        machine_id: String,
        server_id: String,
        alias: String,
        cwd: String,
    },
    Enroll {
        address: TeamAddress,
        alias: String,
    },
    Rename {
        address: TeamAgentTarget,
        alias: String,
    },
    Remove {
        address: TeamAgentTarget,
    },
    Status {
        operation_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
pub enum TeamCleanupParams {
    Preview {
        scope: TeamBoardPurgeScope,
    },
    Apply {
        scope: TeamBoardPurgeScope,
        #[ts(type = "number")]
        expected_revision: i64,
    },
    Status {
        operation_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", content = "request", rename_all = "camelCase")]
#[ts(
    tag = "type",
    content = "request",
    rename_all = "camelCase",
    export_to = "v2/"
)]
pub enum TeamAdministrationRequest {
    Member(TeamMemberActionParams),
    Cleanup(TeamCleanupParams),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TeamAdministrationState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Rejected,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAdministrationOperation {
    pub operation_id: String,
    pub actor: TeamAddress,
    pub request: TeamAdministrationRequest,
    pub state: TeamAdministrationState,
    pub waiting_reason: Option<String>,
    pub result: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TeamMemberRetirementState {
    Retiring,
    Removed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMemberRetirement {
    pub operation_id: String,
    pub address: TeamAddress,
    pub state: TeamMemberRetirementState,
    pub waiting_reason: Option<String>,
}

pub fn team_member_action_schema() -> Value {
    let address = serde_json::json!({"type":"object","properties":{
        "machineId":{"type":"string"},"serverId":{"type":"string"},"threadId":{"type":"string"}
    },"required":["machineId","serverId","threadId"],"additionalProperties":false});
    let target = serde_json::json!({"oneOf":[{"type":"string"},address]});
    serde_json::json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"create","type":"string"},"machineId":{"type":"string"},"serverId":{"type":"string"},"alias":{"type":"string"},"cwd":{"type":"string"}},"required":["type","machineId","serverId","alias","cwd"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"enroll","type":"string"},"address":address,"alias":{"type":"string"}},"required":["type","address","alias"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"rename","type":"string"},"address":target,"alias":{"type":"string"}},"required":["type","address","alias"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"remove","type":"string"},"address":target},"required":["type","address"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"status","type":"string"},"operationId":{"type":"string"}},"required":["type","operationId"],"additionalProperties":false}
    ]})
}

pub fn team_cleanup_schema() -> Value {
    let scope = serde_json::json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"conversation","type":"string"},"conversationId":{"type":"string"}},"required":["type","conversationId"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"topic","type":"string"},"topic":{"type":"string"}},"required":["type","topic"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"all","type":"string"}},"required":["type"],"additionalProperties":false}
    ]});
    serde_json::json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"preview","type":"string"},"scope":scope},"required":["type","scope"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"apply","type":"string"},"scope":scope,"expectedRevision":{"type":"integer"}},"required":["type","scope","expectedRevision"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"status","type":"string"},"operationId":{"type":"string"}},"required":["type","operationId"],"additionalProperties":false}
    ]})
}
