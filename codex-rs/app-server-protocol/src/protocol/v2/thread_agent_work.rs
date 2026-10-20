use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Authoritative native-agent mailbox and barrier state, independent of turn activity.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAgentWork {
    /// Monotonic revision within the current controller instance.
    #[ts(type = "number")]
    pub revision: u64,
    /// Accepted messages not yet included in an accepted model request.
    #[ts(type = "number")]
    pub pending_mail: u64,
    /// The current wait-all observation, if any. This does not own or cancel tasks.
    pub waiting_tasks: Option<ThreadAgentBarrier>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAgentBarrier {
    #[ts(type = "number")]
    pub total: u64,
    #[ts(type = "number")]
    pub remaining: u64,
    #[ts(type = "number")]
    pub held: u64,
    #[ts(type = "number")]
    pub unavailable: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAgentWorkUpdatedNotification {
    pub thread_id: String,
    /// Null means this controller cannot currently supply an authoritative snapshot.
    pub agent_work: Option<ThreadAgentWork>,
}
