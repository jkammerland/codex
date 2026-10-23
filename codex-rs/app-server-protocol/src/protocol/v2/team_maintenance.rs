//! Board maintenance shared by operator commands and enrolled team members.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

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
pub enum TeamBoardPurgeScope {
    Conversation { conversation_id: String },
    Topic { topic: String },
    All,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardPurgePreview {
    pub scope: TeamBoardPurgeScope,
    #[ts(type = "number")]
    pub revision: i64,
    pub conversation_count: u32,
    pub message_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardPurgeWorker {
    pub machine_id: String,
    pub server_id: String,
    pub installation_id: String,
    pub acknowledged: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardPurgeOperation {
    pub operation_id: String,
    pub scope: TeamBoardPurgeScope,
    #[ts(type = "number")]
    pub revision: i64,
    pub conversation_count: u32,
    pub message_count: u32,
    pub workers: Vec<TeamBoardPurgeWorker>,
}

/// Content-free removal evidence is replayed before normal worker intake.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardPurgeJournal {
    pub operation_id: String,
    #[ts(type = "number")]
    pub revision: i64,
    pub conversation_ids: Vec<String>,
    pub message_ids: Vec<String>,
}
