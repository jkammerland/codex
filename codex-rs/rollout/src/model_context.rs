use crate::ResponseItemEnvelope;
use crate::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionMetaLine;
use std::collections::HashMap;
use std::collections::HashSet;

/// Whether a reverse model-context scan needs more rollout items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelContextScanProgress {
    /// The reader should provide the next older rollout item.
    Continue,
    /// The scan has collected a safe bounded suffix.
    Complete,
    /// The durable-state scan exceeded its hard scan or retained-state bound.
    LimitExceeded,
}

/// Maximum number of rollout records inspected while reconstructing durable V2 agent state.
pub const MAX_PRESERVED_DURABLE_SCAN_ITEMS: usize = 262_144;
/// Maximum number of durable-state records retained beyond the bounded model-context suffix.
pub const MAX_PRESERVED_DURABLE_STATE_ITEMS: usize = 16_384;
/// Maximum serialized bytes retained for durable state beyond the model-context suffix.
pub const MAX_PRESERVED_DURABLE_STATE_BYTES: usize = 32 * 1024 * 1024;

/// Selects whether a reverse scan may stop at the newest context checkpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelContextScanMode {
    /// Stop once the latest model context and turn settings are reconstructable.
    #[default]
    Bounded,
    /// Scan toward the rollout head, subject to explicit safety bounds, so durable agent mailbox
    /// state cannot be silently omitted.
    PreserveDurableAgentState,
}

/// Accumulates newest-to-oldest rollout items until they are sufficient to reconstruct the latest
/// model context.
///
/// Storage implementations own how they fetch older items. Local JSONL readers and future
/// reverse-paged cloud readers can both feed their items through this scan to share the cutoff
/// rules and chronological replay assembly.
///
/// The scan stops once it has both:
///
/// - `saw_compaction`: a `CompactedItem` with `replacement_history` and `window_number`;
/// - `saw_completed_turn_context`: a completed user turn with a compatible `TurnContextItem`.
///
/// If the scan reaches the beginning before finding a bounded cutoff, it has already collected
/// the complete replay and so we can return that directly.
///
/// `TurnContextItem` does not identify whether it came from a user turn, so one only counts after
/// the same turn also proves a user-turn boundary: a paginated
/// `ItemCompleted(UserMessage)` marker, agent message, or inter-agent message. Paginated writers
/// persist that marker for real user turns; older rollouts without it conservatively scan to the
/// beginning. A raw `role=user` response item is not sufficient because contextual user fragments
/// use that role but do not count as turn boundaries during reconstruction. The compaction restores
/// model-visible items; the turn context restores previous settings (`model`, `comp_hash`, and
/// `realtime_active`) and the reference baseline.
///
/// These paginated shapes disable the bounded cutoff:
///
/// - compaction without `replacement_history` or `window_number`;
/// - rollback markers;
///
/// When one appears, the scanner continues to the beginning and returns the complete replay.
/// Durable-state scans additionally fail closed after [`MAX_PRESERVED_DURABLE_SCAN_ITEMS`]
/// inspected records, [`MAX_PRESERVED_DURABLE_STATE_ITEMS`] retained records, or
/// [`MAX_PRESERVED_DURABLE_STATE_BYTES`] serialized retained bytes.
#[derive(Debug, Default)]
pub struct ModelContextScan {
    items_newest_first: Vec<RolloutItem>,
    older_durable_items_newest_first: Vec<RolloutItem>,
    older_candidate_response: Option<RolloutItem>,
    older_latest_turn_started: Option<RolloutItem>,
    retained_started_turn_ids: HashSet<String>,
    retained_durable_state_bytes: usize,
    scanned_items: usize,
    saw_compaction: bool,
    saw_completed_turn_context: bool,
    must_scan_to_start: bool,
    active_segment: ActiveTurnSegment,
    mode: ModelContextScanMode,
    bounded_suffix_len: Option<usize>,
}

impl ModelContextScan {
    /// Creates a reverse scan with the requested retention behavior.
    pub fn new(mode: ModelContextScanMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Returns whether the scan found a safe context checkpoint while continuing for durable
    /// agent state.
    pub fn has_bounded_suffix(&self) -> bool {
        self.bounded_suffix_len.is_some()
    }

    /// Adds the next newest-to-oldest rollout item and reports whether the reader can stop.
    pub fn push(&mut self, item: RolloutItem) -> ModelContextScanProgress {
        if self.mode == ModelContextScanMode::PreserveDurableAgentState
            && self.scanned_items >= MAX_PRESERVED_DURABLE_SCAN_ITEMS
        {
            return ModelContextScanProgress::LimitExceeded;
        }
        self.scanned_items = self.scanned_items.saturating_add(1);
        if self.mode == ModelContextScanMode::PreserveDurableAgentState
            && self.bounded_suffix_len.is_some()
        {
            self.observe_older_durable_item(item);
            return if self.older_durable_items_newest_first.len()
                > MAX_PRESERVED_DURABLE_STATE_ITEMS
                || self.retained_durable_state_bytes > MAX_PRESERVED_DURABLE_STATE_BYTES
            {
                ModelContextScanProgress::LimitExceeded
            } else {
                ModelContextScanProgress::Continue
            };
        }
        let progress = self.observe(&item);
        self.items_newest_first.push(item);
        if self.mode == ModelContextScanMode::PreserveDurableAgentState
            && self.bounded_suffix_len.is_none()
            && self.has_context_cutoff()
        {
            self.bounded_suffix_len = Some(self.items_newest_first.len());
        }
        progress
    }

    /// Returns the collected items in chronological order with canonical head metadata.
    ///
    /// Call this after the reader reaches the beginning of its source or after [`Self::push`]
    /// returns [`ModelContextScanProgress::Complete`].
    pub fn finish(mut self, session_meta: SessionMetaLine) -> Vec<RolloutItem> {
        if self.bounded_suffix_len.is_some() {
            self.older_durable_items_newest_first.reverse();
            self.items_newest_first.reverse();
            let durable_state = compact_durable_agent_state(
                self.older_durable_items_newest_first.as_slice(),
                self.items_newest_first.as_slice(),
            );
            let mut items =
                Vec::with_capacity(1 + durable_state.len() + self.items_newest_first.len());
            items.push(RolloutItem::SessionMeta(session_meta));
            items.extend(durable_state);
            items.extend(self.items_newest_first);
            return items;
        }
        self.items_newest_first.reverse();
        if self.has_bounded_cutoff() {
            // A bounded scan stops before reaching the head. Prepend the separately loaded head
            // SessionMeta, which remains canonical when copied fork history contains later
            // metadata.
            self.items_newest_first
                .insert(0, RolloutItem::SessionMeta(session_meta));
        }
        self.items_newest_first
    }

    fn observe_older_durable_item(&mut self, item: RolloutItem) {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {
                self.older_latest_turn_started = Some(item);
                self.older_candidate_response = None;
            }
            RolloutItem::DurableInterAgentCommunicationClaimed { ref turn_id, .. } => {
                if let Some(RolloutItem::EventMsg(EventMsg::TurnStarted(started))) =
                    self.older_latest_turn_started.as_ref()
                    && started.turn_id == *turn_id
                    && self.retained_started_turn_ids.insert(turn_id.clone())
                {
                    self.retain_older_durable_item(RolloutItem::EventMsg(EventMsg::TurnStarted(
                        started.clone(),
                    )));
                }
                self.retain_older_durable_item(item);
                self.older_candidate_response = None;
            }
            RolloutItem::DurableInterAgentCommunication { .. } => {
                self.retain_older_durable_item(item);
                self.older_candidate_response = None;
            }
            RolloutItem::InterAgentCommunicationMetadata {
                ref mail_fingerprint,
                ref consumed_mail_id,
                ..
            } if mail_fingerprint.is_some() || consumed_mail_id.is_some() => {
                if let Some(response) = self.older_candidate_response.take() {
                    self.retain_older_durable_item(response);
                }
                self.retain_older_durable_item(item);
            }
            RolloutItem::ResponseItem(response)
                if response.id().is_some() && response_item_is_durable_mail(&response.item) =>
            {
                self.older_candidate_response = Some(RolloutItem::ResponseItem(response));
            }
            _ => {
                self.older_candidate_response = None;
            }
        }
    }

    fn retain_older_durable_item(&mut self, item: RolloutItem) {
        let serialized_bytes = serde_json::to_vec(&item)
            .map(|serialized| serialized.len())
            .unwrap_or(MAX_PRESERVED_DURABLE_STATE_BYTES.saturating_add(1));
        self.retained_durable_state_bytes = self
            .retained_durable_state_bytes
            .saturating_add(serialized_bytes);
        self.older_durable_items_newest_first.push(item);
    }

    fn observe(&mut self, item: &RolloutItem) -> ModelContextScanProgress {
        if self.must_scan_to_start {
            return ModelContextScanProgress::Continue;
        }

        match item {
            RolloutItem::Compacted(compacted)
                if compacted.replacement_history.is_none() || compacted.window_number.is_none() =>
            {
                self.must_scan_to_start = true;
            }
            RolloutItem::Compacted(_) => {
                self.saw_compaction = true;
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)) => {
                // Paginated threads reject rollback. Keep old rollouts correct rather than
                // duplicating rollback survival semantics in this bounded selector.
                self.must_scan_to_start = true;
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = Some(event.turn_id.clone());
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(event.turn_id.as_str()),
                ) {
                    self.active_segment.has_user_turn |=
                        matches!(&event.item, TurnItem::UserMessage(_));
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                self.active_segment
                    .turn_id
                    .get_or_insert_with(|| event.turn_id.clone());
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                if let Some(turn_id) = &event.turn_id {
                    self.active_segment
                        .turn_id
                        .get_or_insert_with(|| turn_id.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(event.turn_id.as_str()),
                ) {
                    self.finalize_active_segment();
                }
            }
            RolloutItem::TurnContext(context) => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = context.turn_id.clone();
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    context.turn_id.as_deref(),
                ) {
                    self.active_segment.has_turn_context = true;
                }
            }
            RolloutItem::ResponseItem(response_item) => {
                self.active_segment.has_user_turn |=
                    response_item_counts_as_user_turn(response_item);
            }
            RolloutItem::InterAgentCommunication(_) => {
                self.active_segment.has_user_turn = true;
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication: Some(_),
                delivered_response: None,
                turn_id,
                ..
            } => {
                self.active_segment
                    .turn_id
                    .get_or_insert_with(|| turn_id.clone());
                self.active_segment.has_user_turn = true;
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                delivered_response: Some(_),
                turn_id,
                ..
            } => {
                self.active_segment
                    .turn_id
                    .get_or_insert_with(|| turn_id.clone());
                self.active_segment.has_user_turn = true;
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                self.active_segment.has_user_turn = true;
            }
            RolloutItem::EventMsg(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::DurableInterAgentCommunication { .. }
            | RolloutItem::DurableInterAgentCommunicationClaimed {
                communication: None,
                delivered_response: None,
                ..
            }
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::WorldState(_) => {}
        }

        if self.has_bounded_cutoff() {
            ModelContextScanProgress::Complete
        } else {
            ModelContextScanProgress::Continue
        }
    }

    fn finalize_active_segment(&mut self) {
        if self.active_segment.has_user_turn && self.active_segment.has_turn_context {
            self.saw_completed_turn_context = true;
        }
        self.active_segment = ActiveTurnSegment::default();
    }

    fn has_bounded_cutoff(&self) -> bool {
        self.mode == ModelContextScanMode::Bounded && self.has_context_cutoff()
    }

    fn has_context_cutoff(&self) -> bool {
        !self.must_scan_to_start && self.saw_compaction && self.saw_completed_turn_context
    }
}

fn compact_durable_agent_state(
    older_items: &[RolloutItem],
    bounded_suffix: &[RolloutItem],
) -> Vec<RolloutItem> {
    let chronological_items = older_items.iter().chain(bounded_suffix).collect::<Vec<_>>();
    let mut future_turn_starts = HashSet::new();
    let mut delivered_claim_positions = HashSet::new();
    for (index, item) in chronological_items.iter().enumerate().rev() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                future_turn_starts.insert(event.turn_id.as_str());
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                delivered_response,
                turn_id,
                ..
            } if delivered_response.is_some() || future_turn_starts.contains(turn_id.as_str()) => {
                delivered_claim_positions.insert(index);
            }
            _ => {}
        }
    }

    let mut consumed_fingerprints = HashMap::<ResponseItemId, (usize, String)>::new();
    let mut latest_fingerprint_by_id = HashMap::<ResponseItemId, String>::new();
    for (position, item) in chronological_items.iter().enumerate() {
        match item {
            RolloutItem::DurableInterAgentCommunication {
                communication,
                fingerprint,
                ..
            } => {
                if let Some(communication_id) = &communication.id {
                    latest_fingerprint_by_id.insert(communication_id.clone(), fingerprint.clone());
                }
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id,
                fingerprint,
                ..
            } => {
                latest_fingerprint_by_id.insert(communication_id.clone(), fingerprint.clone());
                if delivered_claim_positions.contains(&position) {
                    consumed_fingerprints
                        .entry(communication_id.clone())
                        .or_insert_with(|| (position, fingerprint.clone()));
                }
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: Some(communication),
                consumed_mail_id,
                ..
            } => {
                if let Some(communication_id) = &communication.id {
                    latest_fingerprint_by_id.insert(communication_id.clone(), fingerprint.clone());
                }
                if let Some(communication_id) = consumed_mail_id {
                    consumed_fingerprints
                        .entry(communication_id.clone())
                        .or_insert_with(|| (position, fingerprint.clone()));
                }
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                consumed_mail_id: Some(communication_id),
                ..
            } => {
                consumed_fingerprints
                    .entry(communication_id.clone())
                    .or_insert_with(|| (position, fingerprint.clone()));
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: None,
                consumed_mail_id: Some(communication_id),
                ..
            } => {
                if let Some(fingerprint) = latest_fingerprint_by_id.get(communication_id) {
                    consumed_fingerprints
                        .entry(communication_id.clone())
                        .or_insert_with(|| (position, fingerprint.clone()));
                }
            }
            _ => {}
        }
    }
    let mut mcp10_receipts = HashMap::new();
    for (position, pair) in chronological_items.windows(2).enumerate() {
        let [
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: None,
                consumed_mail_id: None,
                ..
            },
            RolloutItem::ResponseItem(response_item),
        ] = pair
        else {
            continue;
        };
        let Some(communication_id) = response_item.id().cloned() else {
            continue;
        };
        mcp10_receipts.insert(position, (communication_id.clone(), fingerprint.clone()));
        match consumed_fingerprints.get_mut(&communication_id) {
            Some((current_position, current_fingerprint)) if position < *current_position => {
                *current_position = position;
                *current_fingerprint = fingerprint.clone();
            }
            Some(_) => {}
            None => {
                consumed_fingerprints.insert(communication_id, (position, fingerprint.clone()));
            }
        }
    }

    let mut compacted = Vec::new();
    let mut emitted_consumed_ids = HashSet::new();
    for (position, item) in older_items.iter().enumerate() {
        if let Some((communication_id, fingerprint)) = mcp10_receipts.get(&position) {
            if emitted_consumed_ids.insert(communication_id.clone()) {
                compacted.push(consumed_mail_marker(
                    communication_id.clone(),
                    fingerprint.clone(),
                ));
            }
            continue;
        }
        match item {
            RolloutItem::DurableInterAgentCommunication { communication, .. } => {
                let Some(communication_id) = communication.id.clone() else {
                    continue;
                };
                if let Some(consumed) = consumed_fingerprints.get(&communication_id) {
                    emit_compacted_consumed_marker(
                        &mut compacted,
                        &mut emitted_consumed_ids,
                        older_items.len(),
                        &communication_id,
                        consumed,
                    );
                } else {
                    compacted.push(item.clone());
                }
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id, ..
            } => {
                if let Some(consumed) = consumed_fingerprints.get(communication_id) {
                    emit_compacted_consumed_marker(
                        &mut compacted,
                        &mut emitted_consumed_ids,
                        older_items.len(),
                        communication_id,
                        consumed,
                    );
                } else {
                    compacted.push(item.clone());
                }
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_communication,
                consumed_mail_id,
                ..
            } => {
                if let Some(communication_id) = consumed_mail_id
                    && let Some(consumed) = consumed_fingerprints.get(communication_id)
                {
                    emit_compacted_consumed_marker(
                        &mut compacted,
                        &mut emitted_consumed_ids,
                        older_items.len(),
                        communication_id,
                        consumed,
                    );
                }
                let Some(communication_id) = mail_communication
                    .as_ref()
                    .and_then(|communication| communication.id.clone())
                else {
                    continue;
                };
                if let Some(consumed) = consumed_fingerprints.get(&communication_id) {
                    emit_compacted_consumed_marker(
                        &mut compacted,
                        &mut emitted_consumed_ids,
                        older_items.len(),
                        &communication_id,
                        consumed,
                    );
                } else {
                    let mut retained = item.clone();
                    if let RolloutItem::InterAgentCommunicationMetadata {
                        consumed_mail_id, ..
                    } = &mut retained
                    {
                        *consumed_mail_id = None;
                    }
                    compacted.push(retained);
                }
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::RealtimeItem(_) => {}
        }
    }
    compacted
}

fn emit_compacted_consumed_marker(
    compacted: &mut Vec<RolloutItem>,
    emitted_consumed_ids: &mut HashSet<ResponseItemId>,
    bounded_suffix_start: usize,
    communication_id: &ResponseItemId,
    consumed: &(usize, String),
) {
    let (delivery_position, fingerprint) = consumed;
    if *delivery_position < bounded_suffix_start
        && emitted_consumed_ids.insert(communication_id.clone())
    {
        compacted.push(consumed_mail_marker(
            communication_id.clone(),
            fingerprint.clone(),
        ));
    }
}

fn consumed_mail_marker(id: ResponseItemId, fingerprint: String) -> RolloutItem {
    RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
        mail_fingerprint: Some(fingerprint),
        delivery_id: None,
        terminal_turn_id: None,
        terminal_status: None,
        mail_communication: None,
        mail_start_options: None,
        consumed_mail_id: Some(id),
    }
}

#[derive(Debug, Default)]
struct ActiveTurnSegment {
    turn_id: Option<String>,
    has_user_turn: bool,
    has_turn_context: bool,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn response_item_counts_as_user_turn(response_item: &ResponseItemEnvelope) -> bool {
    match &response_item.item {
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Message { role, content, .. } => {
            role == "assistant" && InterAgentCommunication::is_message_content(content)
        }
        _ => false,
    }
}

fn response_item_is_durable_mail(response_item: &ResponseItem) -> bool {
    match response_item {
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Message { content, .. } => {
            InterAgentCommunication::is_message_content(content)
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;
