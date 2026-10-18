//! Passive teammate discovery; observations are never execution authorization.
use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamActivity {
    #[default]
    Unknown,
    Idle,
    Working,
    WaitingForApproval,
    WaitingForInput,
    Unloaded,
    Error,
    Archived,
}

impl From<&super::ThreadStatus> for TeamActivity {
    fn from(status: &super::ThreadStatus) -> Self {
        use super::ThreadActiveFlag;
        use super::ThreadStatus;
        match status {
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) =>
            {
                Self::WaitingForApproval
            }
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) =>
            {
                Self::WaitingForInput
            }
            ThreadStatus::Active { .. } => Self::Working,
            ThreadStatus::Idle => Self::Idle,
            ThreadStatus::NotLoaded => Self::Unloaded,
            ThreadStatus::SystemError => Self::Error,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamSynchronization {
    Current,
    Disconnected,
    #[default]
    Unconfirmed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamReadiness {
    Ready,
    Resumable,
    Busy,
    Held,
    #[default]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamExecutionHold {
    Paused,
    Stopped,
    Manual,
    Reconciliation,
    Continuation,
    ModelError,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAvailability {
    pub synchronization: TeamSynchronization,
    pub activity: TeamActivity,
    pub readiness: TeamReadiness,
    pub holds: Vec<TeamExecutionHold>,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamExecutionEnvironment {
    pub id: String,
    pub current: bool,
    pub next: bool,
    /// Cached executor observations; absent means unknown, never a failed probe.
    pub os: Option<String>,
    pub shell: Option<String>,
    /// Operator declarations for this exact server/environment pair; not verified.
    pub declared_capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamAgentDiscovery {
    pub availability: TeamAvailability,
    pub environments: Vec<TeamExecutionEnvironment>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamDirectoryEntry {
    pub address: super::TeamAddress,
    pub alias: String,
    pub manually_held: bool,
    pub continuation_turn_id: Option<String>,
    pub discovery: TeamAgentDiscovery,
    pub current_model: Option<String>,
    pub current_effort: Option<String>,
    pub next_model: Option<String>,
    pub next_effort: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamDirectorySnapshot {
    #[ts(type = "number")]
    pub revision: i64,
    pub data: Vec<TeamDirectoryEntry>,
}
