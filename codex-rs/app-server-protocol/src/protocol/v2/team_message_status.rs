use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

use super::TeamAddress;
use super::TeamReceipt;
use super::TeamReceiptState;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamObservationFreshness {
    Current,
    LastSynchronized,
    Unconfirmed,
}

impl TeamObservationFreshness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "Current observation",
            Self::LastSynchronized => "Last synchronized observation",
            Self::Unconfirmed => "Unconfirmed",
        }
    }
}

/// Historical intake evidence, independent of every recipient's processing.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamMessageIntake {
    QueuedOnSenderHost,
    AcceptedAtCoordinator,
    Rejected,
}

impl TeamMessageIntake {
    pub fn label(self) -> &'static str {
        match self {
            Self::QueuedOnSenderHost => "Queued on sender host",
            Self::AcceptedAtCoordinator => "Intake accepted at coordinator",
            Self::Rejected => "Delivery rejected",
        }
    }
}

/// Delivery evidence describes transport and turn processing, never fulfillment.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamMessageProgress {
    QueuedOnSenderHost,
    QueuedAtCoordinator,
    ReceivedByRecipientHost,
    AdmittedToTurn,
    TurnEnded,
    TurnInterrupted,
    DeliveryRejected,
    ProcessingUncertain,
}

impl TeamMessageProgress {
    pub fn label(self) -> &'static str {
        match self {
            Self::QueuedOnSenderHost => "Queued on sender host",
            Self::QueuedAtCoordinator => "Queued at coordinator",
            Self::ReceivedByRecipientHost => "Received by recipient host",
            Self::AdmittedToTurn => "Admitted to turn",
            Self::TurnEnded => "Turn ended",
            Self::TurnInterrupted => "Turn interrupted",
            Self::DeliveryRejected => "Delivery rejected",
            Self::ProcessingUncertain => "Processing uncertain",
        }
    }
}

impl From<TeamReceiptState> for TeamMessageProgress {
    fn from(state: TeamReceiptState) -> Self {
        match state {
            TeamReceiptState::Pending => Self::ReceivedByRecipientHost,
            TeamReceiptState::Accepted => Self::AdmittedToTurn,
            TeamReceiptState::Completed => Self::TurnEnded,
            TeamReceiptState::Interrupted => Self::TurnInterrupted,
            TeamReceiptState::Rejected => Self::DeliveryRejected,
            TeamReceiptState::NeedsReconciliation => Self::ProcessingUncertain,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamRecipientMessageStatus {
    pub address: TeamAddress,
    pub name: String,
    pub progress: TeamMessageProgress,
    pub receipt: Option<TeamReceipt>,
    #[serde(default)]
    pub abandonment: Option<super::TeamDeliveryAbandonment>,
    pub freshness: TeamObservationFreshness,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMessageStatus {
    pub message_id: String,
    pub sender: TeamAddress,
    pub sender_name: String,
    #[ts(type = "number | null")]
    pub created_at: Option<i64>,
    pub intake: TeamMessageIntake,
    pub recipients: Vec<TeamRecipientMessageStatus>,
    /// Replies are optional evidence of conversation, not completion receipts.
    pub related_replies: Vec<String>,
    pub detail: Option<String>,
    pub freshness: TeamObservationFreshness,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMessageStatusSnapshot {
    #[ts(type = "number")]
    pub revision: i64,
    pub data: Vec<TeamMessageStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMessageStatusParams {
    #[ts(optional = nullable)]
    pub message_id: Option<String>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamMessageStatusResponse {
    pub data: Vec<TeamMessageStatus>,
    pub next_cursor: Option<String>,
}
