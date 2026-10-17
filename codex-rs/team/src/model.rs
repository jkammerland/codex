use std::collections::BTreeMap;

use codex_app_server_protocol::Model;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamControlState;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamEnrollment;
use codex_app_server_protocol::TeamExecutionMode;
use codex_app_server_protocol::TeamMessage;
use codex_app_server_protocol::TeamMessageKind;
use codex_app_server_protocol::TeamReceipt;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub enrollment: TeamEnrollment,
    #[serde(default)]
    pub runtime: Option<codex_app_server_protocol::TeamAgentRuntime>,
    pub status: String,
    pub current_model: Option<String>,
    pub current_effort: Option<String>,
    pub next_model: Option<String>,
    pub next_effort: Option<String>,
    pub project: Option<String>,
    #[serde(default)]
    pub requested_selection: Option<codex_app_server_protocol::TeamModelSelection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub machine_id: String,
    pub machine_label: String,
    pub server_id: String,
    pub server_label: String,
    pub installation_id: Option<String>,
    pub connected: bool,
    pub applied_revision: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSnapshot {
    pub control: TeamControlState,
    pub servers: Vec<ServerStatus>,
    pub agents: Vec<Agent>,
    pub messages: Vec<MessageRecord>,
    #[serde(default)]
    pub board: codex_app_server_protocol::TeamBoardSnapshot,
    #[serde(default)]
    pub rejected_outbox: Vec<RejectedOutbox>,
}

/// Durable rejected intake, retained for correction without blocking later mail.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedOutbox {
    pub machine_id: String,
    pub server_id: String,
    pub entry: codex_app_server_protocol::TeamOutboxEntry,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    pub message: TeamMessage,
    pub delivery: TeamDeliveryMode,
    pub targets: Option<Vec<TeamAddress>>,
    pub receipts: BTreeMap<String, TeamReceipt>,
    pub published: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub external_url: Option<String>,
}

/// A caller retains this envelope and reuses it when a response is lost.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamOperation {
    pub operation_id: String,
    pub request: TeamRequest,
}

impl TeamOperation {
    pub fn new(request: TeamRequest) -> Self {
        Self {
            operation_id: uuid::Uuid::now_v7().to_string(),
            request,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TeamRequest {
    Board {
        request: codex_app_server_protocol::TeamBoardParams,
    },
    Reply {
        message_id: String,
        body: String,
        #[serde(default, skip_serializing_if = "TeamDeliveryMode::is_next_turn")]
        delivery: TeamDeliveryMode,
    },
    AgentCredential {
        address: TeamAddress,
    },
    GiteaInspect {
        repository: String,
    },
    Snapshot,
    Control {
        mode: TeamExecutionMode,
    },
    Leave {
        address: TeamAddress,
    },
    Enroll {
        address: TeamAddress,
        alias: String,
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
    },
    Subscribe {
        address: TeamAddress,
        topic: String,
        enabled: bool,
    },
    Models {
        machine_id: String,
        server_id: String,
    },
    SelectModel {
        address: TeamAddress,
        model: codex_app_server_protocol::TeamSelection,
        effort: codex_app_server_protocol::TeamSelection,
    },
    SetModel {
        address: TeamAddress,
        model: String,
        effort: Option<String>,
    },
    Retry {
        message_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TeamResponse {
    Board {
        response: codex_app_server_protocol::TeamBoardResponse,
    },
    AgentCredential {
        credential: String,
    },
    Gitea {
        result: serde_json::Value,
    },
    Snapshot {
        snapshot: TeamSnapshot,
    },
    Control {
        control: TeamControlState,
    },
    Agent {
        agent: Box<Agent>,
    },
    Message {
        message: Box<MessageRecord>,
    },
    Models {
        data: Vec<Model>,
    },
    Done,
}

pub(crate) fn address_key(address: &TeamAddress) -> String {
    format!(
        "{}/{}/{}",
        address.machine_id, address.server_id, address.thread_id
    )
}

pub(crate) fn server_key(machine_id: &str, server_id: &str) -> String {
    format!("{machine_id}/{server_id}")
}

/// Normalize root IDs before they become durable routing or deduplication keys.
pub(crate) fn canonical_address(mut address: TeamAddress) -> anyhow::Result<TeamAddress> {
    if !matches!(address.machine_id.as_str(), "operator" | "gitea") {
        address.thread_id = uuid::Uuid::parse_str(&address.thread_id)?.to_string();
    }
    Ok(address)
}
