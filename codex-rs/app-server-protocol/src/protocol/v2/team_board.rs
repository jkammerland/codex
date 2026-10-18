//! Conversation metadata is synchronized beside immutable team message envelopes.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

use super::TeamAddress;
use super::TeamMessage;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema, TS)]
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
pub enum TeamBoardIdentity {
    User,
    Agent { address: TeamAddress },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardAuthor {
    pub identity: TeamBoardIdentity,
    pub name: String,
    pub machine_name: Option<String>,
    /// Historical names that were not recorded must not be presented as original names.
    pub legacy: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardMention {
    pub identity: TeamBoardIdentity,
    pub display: String,
    /// UTF-8 byte offsets into the entry body, including the leading @.
    pub start: u32,
    pub end: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardEntry {
    pub message: TeamMessage,
    pub conversation_id: String,
    #[ts(type = "number")]
    pub append_seq: i64,
    pub author: TeamBoardAuthor,
    pub mentions: Vec<TeamBoardMention>,
    pub notifications: Vec<TeamBoardNotification>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamBoardNotificationReason {
    Mention,
    ConversationFollow,
    TopicSubscription,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardNotification {
    pub identity: TeamBoardIdentity,
    pub reasons: Vec<TeamBoardNotificationReason>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardFollower {
    pub mode: TeamBoardSubscriptionMode,
    pub conversation_id: String,
    pub identity: TeamBoardIdentity,
    pub author: TeamBoardAuthor,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardReadState {
    pub conversation_id: String,
    pub identity: TeamBoardIdentity,
    #[ts(type = "number")]
    pub append_seq: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardTopicSubscription {
    pub mode: TeamBoardSubscriptionMode,
    pub topic: String,
    pub identity: TeamBoardIdentity,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardMember {
    pub author: TeamBoardAuthor,
    pub available: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardSnapshot {
    #[ts(type = "number")]
    pub revision: i64,
    pub entries: Vec<TeamBoardEntry>,
    pub followers: Vec<TeamBoardFollower>,
    pub reads: Vec<TeamBoardReadState>,
    pub topic_subscriptions: Vec<TeamBoardTopicSubscription>,
    pub members: Vec<TeamBoardMember>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardConversation {
    pub id: String,
    pub root: TeamBoardEntry,
    pub latest: TeamBoardEntry,
    pub reply_count: u32,
    pub follower_count: u32,
    pub following: bool,
    pub subscription_mode: TeamBoardSubscriptionMode,
    pub topic_subscription_mode: TeamBoardSubscriptionMode,
    pub unread: bool,
    pub mentioned: bool,
    #[ts(type = "number")]
    pub read_seq: i64,
}

/// Watch is passive visibility. Only Wake participates in delivery routing.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamBoardSubscriptionMode {
    #[default]
    Off,
    Watch,
    Wake,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
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
pub enum TeamBoardSubscriptionTarget {
    Conversation { conversation_id: String },
    Topic { topic: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardEntryPreview {
    pub message_id: String,
    pub conversation_id: String,
    #[ts(type = "number")]
    pub append_seq: i64,
    pub author: TeamBoardAuthor,
    #[ts(type = "number")]
    pub created_at: i64,
    pub excerpt: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardConversationPreview {
    pub id: String,
    pub root: TeamBoardEntryPreview,
    pub latest: TeamBoardEntryPreview,
    pub reply_count: u32,
    pub unread: bool,
}

/// Deterministic activity preview; authors are actual participants, not subscribers.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardTopicOverview {
    pub topic: String,
    pub recent_conversations: Vec<TeamBoardConversationPreview>,
    pub conversation_count: u32,
    pub message_count: u32,
    pub participants: Vec<TeamBoardAuthor>,
    pub latest: Option<TeamBoardEntryPreview>,
    pub unread_count: u32,
    pub subscription_mode: TeamBoardSubscriptionMode,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamBoardFilter {
    #[default]
    All,
    Following,
    Mentions,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
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
pub enum TeamBoardParams {
    #[serde(rename_all = "camelCase")]
    List {
        #[serde(default)]
        filter: TeamBoardFilter,
        #[ts(optional = nullable)]
        query: Option<String>,
        #[ts(optional = nullable)]
        topic: Option<String>,
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
    },
    #[serde(rename_all = "camelCase")]
    Read {
        conversation_id: String,
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
    },
    #[serde(rename_all = "camelCase")]
    Post {
        topic: String,
        body: String,
        mentions: Vec<TeamBoardMention>,
    },
    #[serde(rename_all = "camelCase")]
    Reply {
        conversation_id: String,
        #[ts(optional = nullable)]
        reply_to: Option<String>,
        body: String,
        mentions: Vec<TeamBoardMention>,
    },
    #[serde(rename_all = "camelCase")]
    Follow {
        conversation_id: String,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
        enabled: bool,
    },
    #[serde(rename_all = "camelCase")]
    Followers {
        conversation_id: String,
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    MarkRead {
        conversation_id: String,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
        #[ts(type = "number")]
        append_seq: i64,
    },
    #[serde(rename_all = "camelCase")]
    Topics {
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    TopicOverview {
        #[ts(optional = nullable)]
        cursor: Option<String>,
        #[ts(optional = nullable)]
        limit: Option<u32>,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
    },
    #[serde(rename_all = "camelCase")]
    SubscriptionSet {
        target: TeamBoardSubscriptionTarget,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
        mode: TeamBoardSubscriptionMode,
    },
    #[serde(rename_all = "camelCase")]
    TopicFollow {
        topic: String,
        #[ts(optional = nullable)]
        identity: Option<TeamBoardIdentity>,
        enabled: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamBoardMutationState {
    Queued,
    Applied,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
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
pub enum TeamBoardResponse {
    #[serde(rename_all = "camelCase")]
    List {
        #[ts(type = "number")]
        revision: i64,
        data: Vec<TeamBoardConversation>,
        next_cursor: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Read {
        #[ts(type = "number")]
        revision: i64,
        conversation: Box<TeamBoardConversation>,
        data: Vec<TeamBoardEntry>,
        next_cursor: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Followers {
        #[ts(type = "number")]
        revision: i64,
        data: Vec<TeamBoardFollower>,
        next_cursor: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Topics {
        #[ts(type = "number")]
        revision: i64,
        data: Vec<String>,
        next_cursor: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    TopicOverview {
        #[ts(type = "number")]
        revision: i64,
        data: Vec<TeamBoardTopicOverview>,
        next_cursor: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Mutation {
        #[ts(type = "number")]
        revision: i64,
        state: TeamBoardMutationState,
        id: String,
        conversation_id: Option<String>,
    },
}

/// Native and MCP callers use one schema and the same host-bound identity.
pub fn team_board_tool_schema() -> serde_json::Value {
    let identity = serde_json::json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["user", "agent"]},
            "address": {"type": "object", "properties": {
                "machineId": {"type": "string"}, "serverId": {"type": "string"}, "threadId": {"type": "string"}
            }, "required": ["machineId", "serverId", "threadId"], "additionalProperties": false}
        },
        "required": ["type"],
        "oneOf": [
            {"properties": {"type": {"const": "user"}}},
            {"properties": {"type": {"const": "agent"}}, "required": ["address"]}
        ],
        "additionalProperties": false
    });
    serde_json::json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["list", "read", "post", "reply", "follow", "followers", "markRead", "topics", "topicFollow", "topicOverview", "subscriptionSet"]},
            "filter": {"type": "string", "enum": ["all", "following", "mentions"]},
            "query": {"type": "string"}, "topic": {"type": "string"},
            "cursor": {"type": "string"}, "limit": {"type": "integer", "minimum": 1},
            "identity": identity,
            "mode": {"type": "string", "enum": ["off", "watch", "wake"]},
            "target": {"type": "object", "properties": {
                "type": {"type": "string", "enum": ["conversation", "topic"]},
                "conversationId": {"type": "string"}, "topic": {"type": "string"}
            }, "required": ["type"], "oneOf": [
                {"properties": {"type": {"const": "conversation"}}, "required": ["conversationId"]},
                {"properties": {"type": {"const": "topic"}}, "required": ["topic"]}
            ], "additionalProperties": false},
            "conversationId": {"type": "string"}, "replyTo": {"type": "string"},
            "body": {"type": "string"}, "enabled": {"type": "boolean"},
            "appendSeq": {"type": "integer", "minimum": 0},
            "mentions": {"type": "array", "items": {
                "type": "object", "properties": {
                    "identity": identity, "display": {"type": "string"},
                    "start": {"type": "integer", "minimum": 0}, "end": {"type": "integer", "minimum": 1}
                }, "required": ["identity", "display", "start", "end"], "additionalProperties": false
            }}
        },
        "required": ["type"],
        "oneOf": [
            {"properties": {"type": {"const": "list"}}},
            {"properties": {"type": {"const": "read"}}, "required": ["conversationId"]},
            {"properties": {"type": {"const": "post"}}, "required": ["topic", "body", "mentions"]},
            {"properties": {"type": {"const": "reply"}}, "required": ["conversationId", "body", "mentions"]},
            {"properties": {"type": {"const": "follow"}}, "required": ["conversationId", "enabled"]},
            {"properties": {"type": {"const": "followers"}}, "required": ["conversationId"]},
            {"properties": {"type": {"const": "markRead"}}, "required": ["conversationId", "appendSeq"]},
            {"properties": {"type": {"const": "topics"}}},
            {"properties": {"type": {"const": "topicFollow"}}, "required": ["topic", "enabled"]},
            {"properties": {"type": {"const": "topicOverview"}}},
            {"properties": {"type": {"const": "subscriptionSet"}}, "required": ["target", "mode"]}
        ],
        "additionalProperties": false
    })
}

#[cfg(test)]
#[path = "team_board_tests.rs"]
mod tests;
