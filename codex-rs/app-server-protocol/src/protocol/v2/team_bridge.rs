use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

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
    #[serde(default)]
    #[ts(as = "Option<_>", optional)]
    pub administration: Vec<super::TeamAdministrationOperation>,
    #[serde(default)]
    #[ts(as = "Option<_>", optional)]
    pub retirements: Vec<super::TeamMemberRetirement>,
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
pub enum TeamAgentActionParams {
    MessageStatus {
        query: super::TeamMessageStatusParams,
    },
    ExplainAction {
        query: super::TeamExplainActionParams,
    },
    BoardAction {
        request: super::TeamBoardParams,
    },
    Publish {
        body: String,
        #[ts(optional = nullable)]
        topic: Option<String>,
        #[ts(optional = nullable)]
        conversation_id: Option<String>,
        #[ts(optional = nullable)]
        reply_to: Option<String>,
        #[ts(optional = nullable)]
        mentions: Option<Vec<super::TeamAgentMentionParams>>,
    },
    Member {
        request: super::TeamMemberActionParams,
    },
    Cleanup {
        request: super::TeamCleanupParams,
    },
    Reply {
        message_id: String,
        body: String,
        #[serde(default)]
        #[ts(as = "Option<_>", optional)]
        delivery: TeamDeliveryMode,
    },
    Directory {
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
        #[ts(optional = nullable)]
        query: Option<String>,
        #[serde(default)]
        #[ts(as = "Option<_>", optional)]
        details: bool,
    },
    Inbox {
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
    },
    Send {
        recipient: super::TeamAgentTarget,
        body: String,
        #[serde(default = "default_message_kind")]
        #[ts(as = "Option<_>", optional)]
        kind: TeamMessageKind,
        #[ts(optional = nullable)]
        reply_to: Option<String>,
        #[serde(default)]
        #[ts(as = "Option<_>", optional)]
        delivery: TeamDeliveryMode,
    },
    Post {
        topic: String,
        body: String,
        #[ts(optional = nullable)]
        reply_to: Option<String>,
    },
    Board {
        topic: String,
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
    },
    Subscribe {
        topic: String,
        enabled: bool,
    },
}

fn default_message_kind() -> TeamMessageKind {
    TeamMessageKind::Request
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
    pub action: TeamAgentActionParams,
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
