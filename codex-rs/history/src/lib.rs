//! Model-history and persisted-rollout domain types.

use std::borrow::Borrow;
use std::io::Write;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::mcp::McpResourceOriginCheckpoint;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::realtime::RealtimeItem;
use codex_protocol::security_risk::SecurityRiskScore;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;

/// A model-history item with room for history-only metadata.
///
/// Persistence keeps the response item intact and stores its metadata separately.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseItemEnvelope {
    pub item: ResponseItem,
    pub metadata: Option<CodexHarnessMetadata>,
}

/// Metadata owned by the Codex harness and persisted with a response item.
///
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct CodexHarnessMetadata {
    /// Whether a developer message was supplied by an app-server client.
    #[serde(default)]
    pub client_authored: bool,
}

impl ResponseItemEnvelope {
    /// Wraps a raw Responses API item for persisted history.
    pub fn new(item: ResponseItem) -> Self {
        Self {
            item,
            metadata: None,
        }
    }

    /// Unwraps the raw Responses API item.
    pub fn into_item(self) -> ResponseItem {
        self.item
    }
}

impl From<ResponseItem> for ResponseItemEnvelope {
    fn from(item: ResponseItem) -> Self {
        Self::new(item)
    }
}

impl Deref for ResponseItemEnvelope {
    type Target = ResponseItem;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl DerefMut for ResponseItemEnvelope {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

impl Borrow<ResponseItem> for ResponseItemEnvelope {
    fn borrow(&self) -> &ResponseItem {
        &self.item
    }
}

/// Persisted rollout item used by core history and rollout storage.
#[derive(Debug, Clone)]
pub enum RolloutItem {
    SessionMeta(SessionMetaLine),
    ResponseItem(ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
    InterAgentCommunicationMetadata {
        trigger_turn: bool,
        /// Stable identity for the exact durable mail payload and turn-start options.
        mail_fingerprint: Option<String>,
        /// Deterministic message ID after a spawned child's terminal outbox entry is handled.
        /// This acknowledgement is never model-visible.
        delivery_id: Option<ResponseItemId>,
        /// Legacy `.mcp.11` terminal marker retained for rollout recovery only.
        terminal_turn_id: Option<String>,
        /// Legacy `.mcp.11` terminal status retained for rollout recovery only.
        terminal_status: Option<AgentStatus>,
        /// Legacy `.mcp.11` durable mailbox payload retained for rollout recovery only.
        mail_communication: Option<InterAgentCommunication>,
        /// Legacy `.mcp.11` mailbox settings retained for rollout recovery only.
        mail_start_options: Option<codex_protocol::turn_input::TurnStartOptions>,
        /// Legacy `.mcp.11` consumed-mail marker retained for rollout recovery only.
        consumed_mail_id: Option<ResponseItemId>,
    },
    /// Model-invisible terminal child outbox entry retained for wire compatibility.
    TerminalAgentStatus {
        turn_id: String,
        status: AgentStatus,
    },
    /// Durable mailbox payload retained until a specific turn claims it.
    DurableInterAgentCommunication {
        communication: InterAgentCommunication,
        start_options: codex_protocol::turn_input::TurnStartOptions,
        fingerprint: String,
        turn_id: String,
        /// Whether this record is pending-only rather than a legacy delivery boundary.
        /// Any adjacent matching response item is a compatibility shadow, not a delivery.
        compatibility_shadow: bool,
    },
    /// Durable proof that a turn committed one exact mailbox delivery occurrence.
    DurableInterAgentCommunicationClaimed {
        communication_id: ResponseItemId,
        /// Bounded model-visible communication stored atomically with the claim.
        /// Older fork rollouts omit this field and recover it from the payload record.
        communication: Option<InterAgentCommunication>,
        /// Exact stamped model item at the real delivery position.
        ///
        /// Older fork rollouts omit this field and use their historical claim semantics.
        delivered_response: Option<ResponseItemEnvelope>,
        fingerprint: String,
        turn_id: String,
    },
    Compacted(CompactedItem),
    TurnContext(TurnContextItem),
    WorldState(WorldStateItem),
    SecurityRiskScore(SecurityRiskScore),
    EventMsg(EventMsg),
    /// Sparse, model-invisible facts used to reconstruct realtime presentation.
    RealtimeItem(RealtimeItem),
}

/// Projects the model-visible response items from a persisted rollout stream.
///
/// New durable mailbox delivery atomically stores the exact delivered response on the claim
/// record. Transitional rollouts may also contain an adjacent compatibility shadow. Consumers
/// that rebuild a model transcript must suppress such a shadow and expose the claimed response at
/// its actual delivery position.
#[derive(Debug, Default)]
pub struct RolloutModelResponseProjection {
    compatibility_shadow_id: Option<ResponseItemId>,
    legacy_claim_shadow_id: Option<ResponseItemId>,
    delivered_communication_ids: std::collections::HashSet<ResponseItemId>,
    pending_legacy_claims:
        std::collections::HashMap<String, Vec<(ResponseItemId, ResponseItemEnvelope)>>,
    pending_legacy_claim_count: usize,
}

const MAX_LEGACY_CLAIMS_PER_TURN: usize = 64;
const MAX_BUFFERED_LEGACY_CLAIMS: usize = 4_096;
const MAX_DURABLE_MODEL_ITEM_BYTES: usize = 10_000;

struct ModelItemSizeWriter {
    remaining: usize,
}

impl Write for ModelItemSizeWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.remaining {
            return Err(std::io::Error::other(
                "durable model item exceeds its persisted-history bound",
            ));
        }
        self.remaining -= buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_durable_model_response(response: &ResponseItemEnvelope, max_bytes: usize) -> bool {
    let mut writer = ModelItemSizeWriter {
        // A byte-level tokenizer cannot produce more tokens than the serialized UTF-8 byte
        // length. Bounding the complete serialized item is therefore a conservative hard token
        // bound independent of model-specific token density.
        remaining: max_bytes,
    };
    serde_json::to_writer(&mut writer, &response.item).is_ok()
}

fn exact_durable_model_response(
    communication_id: &ResponseItemId,
    communication: &InterAgentCommunication,
    response: &ResponseItemEnvelope,
) -> bool {
    communication.id.as_ref() == Some(communication_id)
        && response == &ResponseItemEnvelope::new(communication.to_model_input_item())
        && bounded_durable_model_response(response, MAX_DURABLE_MODEL_ITEM_BYTES)
}

impl RolloutModelResponseProjection {
    /// Applies one rollout item in persistence order and returns its model-visible responses.
    ///
    /// Legacy `.mcp.13` rollouts persisted all durable claims before their `TurnStarted` event.
    /// Those claims are buffered until that event proves the delivery occurred. A turn can claim
    /// more than one mailbox item, so one rollout item can release multiple responses.
    pub fn project<'a>(
        &mut self,
        item: &'a RolloutItem,
    ) -> Vec<std::borrow::Cow<'a, ResponseItemEnvelope>> {
        let compatibility_shadow_id = self.compatibility_shadow_id.take();
        let legacy_claim_shadow_id = self.legacy_claim_shadow_id.take();
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(started)) => {
                let pending = self
                    .pending_legacy_claims
                    .remove(&started.turn_id)
                    .unwrap_or_default();
                self.pending_legacy_claim_count = self
                    .pending_legacy_claim_count
                    .saturating_sub(pending.len());
                pending
                    .into_iter()
                    .filter_map(|(communication_id, response)| {
                        self.delivered_communication_ids
                            .insert(communication_id)
                            .then_some(std::borrow::Cow::Owned(response))
                    })
                    .collect()
            }
            RolloutItem::DurableInterAgentCommunication { communication, .. }
                if communication.id.is_some()
                    || legacy_claim_shadow_id.as_ref() == communication.id.as_ref() =>
            {
                self.compatibility_shadow_id = communication.id.clone();
                Vec::new()
            }
            RolloutItem::ResponseItem(response)
                if compatibility_shadow_id
                    .as_ref()
                    .is_some_and(|shadow_id| Some(shadow_id) == response.id()) =>
            {
                Vec::new()
            }
            RolloutItem::ResponseItem(response) => {
                vec![std::borrow::Cow::Borrowed(response)]
            }
            RolloutItem::InterAgentCommunicationMetadata {
                consumed_mail_id: Some(communication_id),
                ..
            } => {
                if self
                    .delivered_communication_ids
                    .insert(communication_id.clone())
                {
                    let mut removed = 0usize;
                    for pending in self.pending_legacy_claims.values_mut() {
                        let previous_len = pending.len();
                        pending.retain(|(pending_id, _)| pending_id != communication_id);
                        removed =
                            removed.saturating_add(previous_len.saturating_sub(pending.len()));
                    }
                    self.pending_legacy_claim_count =
                        self.pending_legacy_claim_count.saturating_sub(removed);
                }
                Vec::new()
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id,
                communication: Some(communication),
                delivered_response: Some(response),
                ..
            } if exact_durable_model_response(communication_id, communication, response)
                && self
                    .delivered_communication_ids
                    .insert(communication_id.clone()) =>
            {
                vec![std::borrow::Cow::Borrowed(response)]
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id,
                communication: Some(communication),
                delivered_response: None,
                turn_id,
                ..
            } => {
                let mut communication = communication.clone();
                if communication
                    .id
                    .as_ref()
                    .is_some_and(|id| id != communication_id)
                {
                    return Vec::new();
                }
                communication
                    .id
                    .get_or_insert_with(|| communication_id.clone());
                let response = communication.to_model_input_item().into();
                self.legacy_claim_shadow_id = Some(communication_id.clone());
                // Historical admission capped each mailbox turn at 64 items. Keep a second global
                // cap so a malformed rollout containing unmatched claims cannot grow this replay
                // buffer without bound. Excess claims fail closed and stay out of model context.
                if bounded_durable_model_response(&response, MAX_DURABLE_MODEL_ITEM_BYTES)
                    && self.pending_legacy_claim_count < MAX_BUFFERED_LEGACY_CLAIMS
                    && !self.delivered_communication_ids.contains(communication_id)
                {
                    let pending = self
                        .pending_legacy_claims
                        .entry(turn_id.clone())
                        .or_default();
                    if pending.len() < MAX_LEGACY_CLAIMS_PER_TURN {
                        pending.push((communication_id.clone(), response));
                        self.pending_legacy_claim_count += 1;
                    }
                }
                Vec::new()
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::DurableInterAgentCommunication { .. }
            | RolloutItem::DurableInterAgentCommunicationClaimed { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::RealtimeItem(_) => Vec::new(),
        }
    }
}

impl Serialize for RolloutItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        rollout_payload::RolloutItemWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RolloutItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        rollout_payload::RolloutItemWire::deserialize(deserializer).map(Into::into)
    }
}

impl JsonSchema for RolloutItem {
    fn schema_name() -> String {
        "RolloutItem".to_string()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::RolloutItem"))
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        rollout_payload::RolloutItemWire::json_schema(generator)
    }
}

mod rollout_payload;

#[derive(Clone, Debug, PartialEq)]
pub struct CompactedItem {
    pub message: String,
    pub replacement_history: Option<Vec<ResponseItemEnvelope>>,
    pub mcp_resource_origins: Option<McpResourceOriginCheckpoint>,
    pub window_number: Option<u64>,
    pub first_window_id: Option<String>,
    pub previous_window_id: Option<String>,
    pub window_id: Option<String>,
}

impl Serialize for CompactedItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        rollout_payload::CompactedItemWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompactedItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        rollout_payload::CompactedItemWire::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
}

impl JsonSchema for CompactedItem {
    fn schema_name() -> String {
        "CompactedItem".to_string()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::CompactedItem"))
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        rollout_payload::CompactedItemWire::json_schema(generator)
    }
}

impl From<CompactedItem> for ResponseItem {
    fn from(value: CompactedItem) -> Self {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: value.message,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, JsonSchema)]
pub struct RolloutLine {
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(flatten)]
    pub item: RolloutItem,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResumedHistory {
    pub conversation_id: ThreadId,
    pub history: Arc<Vec<RolloutItem>>,
    pub rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum InitialHistory {
    New,
    Cleared,
    Resumed(ResumedHistory),
    Forked(Vec<RolloutItem>),
}

impl InitialHistory {
    pub fn scan_rollout_items(&self, mut predicate: impl FnMut(&RolloutItem) -> bool) -> bool {
        match self {
            Self::New | Self::Cleared => false,
            Self::Resumed(resumed) => resumed.history.iter().any(&mut predicate),
            Self::Forked(items) => items.iter().any(predicate),
        }
    }

    pub fn forked_from_id(&self) -> Option<ThreadId> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.forked_from_id,
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id),
                _ => None,
            }),
        }
    }

    pub fn session_cwd(&self) -> Option<PathBuf> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => session_cwd_from_items(&resumed.history),
            Self::Forked(items) => session_cwd_from_items(items),
        }
    }

    pub fn get_rollout_items(&self) -> &[RolloutItem] {
        match self {
            Self::New | Self::Cleared => &[],
            Self::Resumed(resumed) => &resumed.history,
            Self::Forked(items) => items,
        }
    }

    pub fn get_event_msgs(&self) -> Option<Vec<EventMsg>> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => Some(
                resumed
                    .history
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(event) => Some(event.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            Self::Forked(items) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(event) => Some(event.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
        }
    }

    pub fn get_base_instructions(&self) -> Option<BaseInstructions> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_dynamic_tools(&self) -> Option<Vec<DynamicToolSpec>> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_selected_capability_roots(&self) -> Vec<SelectedCapabilityRoot> {
        self.get_session_meta()
            .map(|meta| meta.selected_capability_roots.clone())
            .unwrap_or_default()
    }

    pub fn get_multi_agent_version(&self) -> Option<MultiAgentVersion> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => {
                multi_agent_version_from_items(&resumed.history, Some(resumed.conversation_id))
            }
            Self::Forked(items) => multi_agent_version_from_items(items, /*thread_id*/ None),
        }
    }

    pub fn get_history_mode(&self, default_history_mode: ThreadHistoryMode) -> ThreadHistoryMode {
        match self {
            Self::New | Self::Cleared => default_history_mode,
            Self::Resumed(_) | Self::Forked(_) => self
                .get_session_meta()
                .map(|meta| meta.history_mode)
                .unwrap_or(default_history_mode),
        }
    }

    pub fn get_resumed_session_sources(&self) -> Option<(SessionSource, Option<ThreadSource>)> {
        let meta = self.get_resumed_session_meta()?;
        Some((meta.source.clone(), meta.thread_source.clone()))
    }

    pub fn get_resumed_thread_source(&self) -> Option<ThreadSource> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.thread_source.clone())
    }

    pub fn get_session_originator(&self) -> Option<String> {
        self.get_session_meta()
            .map(|meta| meta.originator.clone())
            .filter(|originator| !originator.is_empty())
    }

    pub fn get_resumed_parent_thread_id(&self) -> Option<ThreadId> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.parent_thread_id)
    }

    fn get_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            Self::New | Self::Cleared => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
            Self::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
        }
    }

    fn get_resumed_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            Self::New | Self::Cleared | Self::Forked(_) => None,
            Self::Resumed(resumed) => resumed.history.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
        }
    }
}

fn session_cwd_from_items(items: &[RolloutItem]) -> Option<PathBuf> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.cwd.clone()),
        _ => None,
    })
}

fn multi_agent_version_from_items(
    items: &[RolloutItem],
    thread_id: Option<ThreadId>,
) -> Option<MultiAgentVersion> {
    let session_meta_version = items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line)
            if thread_id.is_none_or(|thread_id| meta_line.meta.id == thread_id) =>
        {
            meta_line.meta.multi_agent_version
        }
        _ => None,
    });

    session_meta_version.or_else(|| {
        items.iter().rev().find_map(|item| match item {
            RolloutItem::TurnContext(turn_context) => turn_context.multi_agent_version,
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::DurableInterAgentCommunication { .. }
            | RolloutItem::DurableInterAgentCommunicationClaimed { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::EventMsg(_) => None,
        })
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
