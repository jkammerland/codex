use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::TeamAddress;
use super::TeamDeliveryMode;
use super::TeamMessage;
use super::TeamMessageKind;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamOutboxEntry {
    #[ts(type = "number")]
    pub sequence: i64,
    pub id: String,
    pub kind: String,
    pub payload: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamOutboxListParams {
    pub coordinator_id: String,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamOutboxListResponse {
    pub data: Vec<TeamOutboxEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamOutboxAckParams {
    pub coordinator_id: String,
    #[ts(type = "number[]")]
    pub sequences: Vec<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamOutboxAckResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamSyncParams {
    pub coordinator_id: String,
    pub directory: super::TeamDirectorySnapshot,
    pub board: Vec<TeamMessage>,
    pub board_state: super::TeamBoardSnapshot,
    pub cancellation_adapters: Vec<super::TeamCancellationAdapter>,
    pub message_status: super::TeamMessageStatusSnapshot,
    pub board_purges: Vec<super::TeamBoardPurgeJournal>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamSyncResponse {
    pub board_purge_acknowledgements: Vec<String>,
}

/// Agent actions deliberately omit execution controls and model settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
pub enum TeamAgentAction {
    MessageStatus {
        query: super::TeamMessageStatusParams,
    },
    ExplainAction {
        query: super::TeamExplainActionParams,
    },
    BoardAction {
        request: super::TeamBoardParams,
    },
    Reply {
        message_id: String,
        body: String,
        #[serde(default)]
        delivery: TeamDeliveryMode,
    },
    Directory {
        cursor: Option<String>,
        limit: Option<u32>,
    },
    Inbox {
        cursor: Option<String>,
        limit: Option<u32>,
    },
    Send {
        recipient: TeamAddress,
        body: String,
        kind: TeamMessageKind,
        reply_to: Option<String>,
        delivery: TeamDeliveryMode,
    },
    Post {
        topic: String,
        body: String,
        reply_to: Option<String>,
    },
    Board {
        topic: String,
        cursor: Option<String>,
        limit: Option<u32>,
    },
    Subscribe {
        topic: String,
        enabled: bool,
    },
}

/// Shared native/MCP tool schema. Production protocol derives are intentionally
/// disabled, so the small agent-facing contract has an explicit runtime schema.
pub fn team_agent_tool_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["directory", "inbox", "send", "reply", "post", "board", "subscribe", "boardAction", "messageStatus", "explainAction"]},
            "query": {"type":"object", "description":"messageStatus: messageId, cursor, limit. explainAction: action and optional target address.", "properties": {
                "messageId":{"type":"string"}, "cursor":{"type":"string"}, "limit":{"type":"integer", "minimum":1,"maximum":1000},
                "action":{"type":"string","enum":["enroll","queueMessage","processMessages","post","subscribe","selectModel","pause","resume","stop","leave"]},
                "target":{"type":"object","properties":{"machineId":{"type":"string"},"serverId":{"type":"string"},"threadId":{"type":"string"}},"required":["machineId","serverId","threadId"],"additionalProperties":false}
            },"additionalProperties":false},
            "request": super::team_board_tool_schema(),
            "messageId": {"type": "string", "description": "Message from your inbox to reply to, including its original Gitea conversation."},
            "cursor": {"type": "string", "description": "Cursor from a directory, inbox or board page."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 1000},
            "recipient": {"type": "object", "properties": {
                "machineId": {"type": "string"}, "serverId": {"type": "string"}, "threadId": {"type": "string"}
            }, "required": ["machineId", "serverId", "threadId"], "additionalProperties": false},
            "body": {"type": "string"},
            "kind": {"type": "string", "enum": ["request", "reply", "note"]},
            "replyTo": {"type": "string", "description": "Original message id when replying."},
            "delivery": {"type": "string", "enum": ["nextTurn", "safeBoundary"], "description": "Choose when a request or reply enters the recipient's input. Replies default to nextTurn when omitted; safeBoundary may steer active work at a safe boundary."},
            "topic": {"type": "string"},
            "enabled": {"type": "boolean"}
        },
        "required": ["type"],
        "oneOf": [
            {"properties":{"type":{"const":"messageStatus"}},"required":["query"]},
            {"properties":{"type":{"const":"explainAction"},"query":{"required":["action"]}},"required":["query"]},
            {"properties": {"type": {"const": "boardAction"}}, "required": ["request"]},
            {"properties": {"type": {"const": "reply"}}, "required": ["messageId", "body"]},
            {"properties": {"type": {"const": "directory"}}},
            {"properties": {"type": {"const": "inbox"}}},
            {"properties": {"type": {"const": "send"}}, "required": ["recipient", "body", "kind", "delivery"]},
            {"properties": {"type": {"const": "post"}}, "required": ["topic", "body"]},
            {"properties": {"type": {"const": "board"}}, "required": ["topic"]},
            {"properties": {"type": {"const": "subscribe"}}, "required": ["topic", "enabled"]}
        ],
        "additionalProperties": false
    })
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentRequestParams {
    pub thread_id: String,
    pub credential: String,
    /// Stable logical operation identity for clients that retry mutations.
    #[ts(optional = nullable)]
    pub operation_id: Option<String>,
    pub action: TeamAgentAction,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentRequestResponse {
    pub result: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentCredentialParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentCredentialResponse {
    pub credential: String,
}
