use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

use super::TeamAddress;
use super::TeamObservationFreshness;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamExplainableAction {
    Enroll,
    QueueMessage,
    ProcessMessages,
    Post,
    Subscribe,
    SelectModel,
    Pause,
    Resume,
    Stop,
    Leave,
}

impl TeamExplainableAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Enroll => "Enroll task",
            Self::QueueMessage => "Queue message",
            Self::ProcessMessages => "Process messages",
            Self::Post => "Post to Board",
            Self::Subscribe => "Change subscription",
            Self::SelectModel => "Select model",
            Self::Pause => "Pause",
            Self::Resume => "Resume",
            Self::Stop => "Stop",
            Self::Leave => "Leave team",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamActionAvailability {
    Available,
    Blocked,
    RequiresAuthorization,
    Unconfirmed,
}

impl TeamActionAvailability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Blocked => "Blocked",
            Self::RequiresAuthorization => "Requires authorization",
            Self::Unconfirmed => "Unconfirmed",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamExplainActionParams {
    pub action: TeamExplainableAction,
    #[ts(optional = nullable)]
    pub target: Option<TeamAddress>,
}

/// An observation only. The existing owning action must recheck admission.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamActionExplanation {
    pub action: TeamExplainableAction,
    pub target: Option<TeamAddress>,
    pub availability: TeamActionAvailability,
    pub reason: String,
    pub owner: String,
    pub resolution: Option<String>,
    pub freshness: TeamObservationFreshness,
}
