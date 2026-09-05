use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_code_mode::CellId;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use tokio::sync::watch;

use super::PUBLIC_TOOL_NAME;
use crate::context::ContextualUserFragment;
use crate::context::HookAdditionalContext;

const MAX_CODE_MODE_INPUT_ITEMS: usize = 64;
const MAX_CODE_MODE_INPUT_ITEM_TOKENS: usize = 900;
const MAX_CODE_MODE_INPUT_TOTAL_TOKENS: usize = 4_000;
pub(crate) const MAX_CODE_MODE_INPUT_TEXT_BYTES: usize = 4 * 1024;
pub(crate) const MAX_CODE_MODE_INPUT_TOTAL_BYTES: usize = 16 * 1024;
const TRUNCATION_MARKER: &str = "\n[code-mode input truncated]";

pub(super) struct DeferredCellInputRegistry {
    state: Arc<Mutex<DeferredCellInputState>>,
    activity_tx: watch::Sender<u64>,
}

#[derive(Default)]
struct DeferredCellInputState {
    active_cells: HashSet<CellId>,
    entries: VecDeque<DeferredCellInputEntry>,
    // Terminal cells release their active quota immediately, but retain one bounded, one-shot
    // delivery opportunity so a later wait can surface inputs produced after the prior turn.
    terminal_entries: VecDeque<DeferredCellInputEntry>,
    retained_bytes: usize,
    terminal_retained_bytes: usize,
    // Charges survive prompt snapshots and retries until an actual model stream accepts the
    // corresponding item ID. Delivery state distinguishes a raced injection from an item that
    // really entered history, so a new-turn acknowledgement cannot release in-flight quota.
    admitted: CellInputQuota,
    outstanding: HashMap<ResponseItemId, OutstandingCellInput>,
}

struct OutstandingCellInput {
    cell_id: CellId,
    tokens: usize,
    upper_bound_tokens: usize,
    turn_id: String,
    delivery: CellInputDelivery,
}

pub(crate) struct AdmittedCellInputBatch {
    cell_id: CellId,
    inputs: Vec<DeferredCellInput>,
    state: Weak<Mutex<DeferredCellInputState>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CellInputDelivery {
    InFlight,
    Deferred,
    Delivered,
}

#[derive(Clone, Copy, Default)]
struct CellInputQuota {
    items: usize,
    tokens: usize,
    upper_bound_tokens: usize,
}

struct DeferredCellInputEntry {
    cell_id: CellId,
    input: DeferredCellInput,
    retained_bytes: usize,
}

#[derive(Clone, Copy)]
enum DeferredCellInputKind {
    Notification,
    HookContext,
}

#[derive(Clone)]
pub(super) struct DeferredCellInput {
    kind: DeferredCellInputKind,
    response_item: ResponseItem,
    fixed_retained_bytes: usize,
}

impl DeferredCellInputRegistry {
    pub(super) fn open_cell(&self, cell_id: &CellId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_cells
            .insert(cell_id.clone());
    }

    pub(super) fn acknowledge_model_sample(&self, input: &[ResponseItem], turn_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sampled_ids = input
            .iter()
            .filter_map(ResponseItem::id)
            .cloned()
            .collect::<HashSet<_>>();
        // A new turn cannot consume retained input stamped for an ended turn. Delivered old-turn
        // items are either present in `input` or no longer eligible for a later request.
        let entries = std::mem::take(&mut state.entries);
        let mut retained = VecDeque::with_capacity(entries.len());
        for entry in entries {
            if sampled_ids.contains(entry.input.item_id()) {
                state.retained_bytes = state.retained_bytes.saturating_sub(entry.retained_bytes);
                state.release_admission(entry.input.item_id());
            } else {
                retained.push_back(entry);
            }
        }
        state.entries = retained;
        let releasable_ids = state
            .outstanding
            .iter()
            .filter_map(|(item_id, outstanding)| {
                (sampled_ids.contains(item_id)
                    || (outstanding.turn_id != turn_id
                        && outstanding.delivery == CellInputDelivery::Delivered))
                    .then_some(item_id.clone())
            })
            .collect::<Vec<_>>();
        for item_id in releasable_ids {
            state.release_admission(&item_id);
        }
    }

    pub(super) fn take_admitted(&self, cell_id: &CellId) -> Option<AdmittedCellInputBatch> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries = std::mem::take(&mut state.entries);
        let mut retained = VecDeque::with_capacity(entries.len());
        let mut inputs = Vec::new();
        let mut removed_bytes = 0_usize;
        for entry in entries {
            if &entry.cell_id == cell_id {
                removed_bytes = removed_bytes.saturating_add(entry.retained_bytes);
                if let Some(outstanding) = state.outstanding.get_mut(entry.input.item_id()) {
                    outstanding.delivery = CellInputDelivery::InFlight;
                }
                inputs.push(entry.input);
            } else {
                retained.push_back(entry);
            }
        }
        state.entries = retained;
        state.retained_bytes = state.retained_bytes.saturating_sub(removed_bytes);
        let terminal_entries = std::mem::take(&mut state.terminal_entries);
        let mut retained_terminal = VecDeque::with_capacity(terminal_entries.len());
        let mut removed_terminal_bytes = 0_usize;
        for entry in terminal_entries {
            if &entry.cell_id == cell_id {
                removed_terminal_bytes =
                    removed_terminal_bytes.saturating_add(entry.retained_bytes);
                if let Some(input) = state.try_admit_input(cell_id, entry.input) {
                    inputs.push(input);
                }
            } else {
                retained_terminal.push_back(entry);
            }
        }
        state.terminal_entries = retained_terminal;
        state.terminal_retained_bytes = state
            .terminal_retained_bytes
            .saturating_sub(removed_terminal_bytes);
        (!inputs.is_empty()).then(|| AdmittedCellInputBatch {
            cell_id: cell_id.clone(),
            inputs,
            state: Arc::downgrade(&self.state),
        })
    }

    pub(super) fn defer_admitted(&self, batch: AdmittedCellInputBatch) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.active_cells.contains(&batch.cell_id) {
            for input in &batch.inputs {
                state.release_admission(input.item_id());
            }
            drop(state);
            return;
        }
        for input in &batch.inputs {
            let Some(outstanding) = state.outstanding.get_mut(input.item_id()) else {
                continue;
            };
            if outstanding.cell_id != batch.cell_id {
                continue;
            }
            outstanding.delivery = CellInputDelivery::Deferred;
            let entry = DeferredCellInputEntry::new(batch.cell_id.clone(), input.clone());
            state.retained_bytes = state.retained_bytes.saturating_add(entry.retained_bytes);
            state.entries.push_back(entry);
        }
        state.enforce_bounds_from_front();
        drop(state);
        self.signal_activity();
    }

    pub(super) fn restore(&self, batch: AdmittedCellInputBatch) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.active_cells.contains(&batch.cell_id) {
            for input in &batch.inputs {
                state.release_admission(input.item_id());
            }
            drop(state);
            return;
        }
        for input in batch.inputs.iter().rev() {
            let Some(outstanding) = state.outstanding.get_mut(input.item_id()) else {
                continue;
            };
            if outstanding.cell_id != batch.cell_id {
                continue;
            }
            outstanding.delivery = CellInputDelivery::Deferred;
            let entry = DeferredCellInputEntry::new(batch.cell_id.clone(), input.clone());
            state.retained_bytes = state.retained_bytes.saturating_add(entry.retained_bytes);
            state.entries.push_front(entry);
        }
        state.enforce_bounds_from_back();
        drop(state);
        self.signal_activity();
    }

    pub(super) fn subscribe_activity(&self) -> watch::Receiver<u64> {
        self.activity_tx.subscribe()
    }

    pub(super) fn has_pending_for_cell(&self, cell_id: &CellId) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entries
            .iter()
            .chain(state.terminal_entries.iter())
            .any(|entry| &entry.cell_id == cell_id)
    }

    pub(super) fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = DeferredCellInputState::default();
        drop(state);
        self.signal_activity();
    }

    pub(super) fn discard_cell(&self, cell_id: &CellId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_active = state.active_cells.remove(cell_id);
        let previous_entry_count = state.entries.len();
        let entries = std::mem::take(&mut state.entries);
        for entry in entries {
            if &entry.cell_id == cell_id {
                state.retained_bytes = state.retained_bytes.saturating_sub(entry.retained_bytes);
                state.release_admission(entry.input.item_id());
                state.terminal_retained_bytes = state
                    .terminal_retained_bytes
                    .saturating_add(entry.retained_bytes);
                state.terminal_entries.push_back(entry);
            } else {
                state.entries.push_back(entry);
            }
        }
        while state.terminal_entries.len() > MAX_CODE_MODE_INPUT_ITEMS
            || state.terminal_retained_bytes > MAX_CODE_MODE_INPUT_TOTAL_BYTES
        {
            let Some(entry) = state.terminal_entries.pop_front() else {
                break;
            };
            state.terminal_retained_bytes = state
                .terminal_retained_bytes
                .saturating_sub(entry.retained_bytes);
        }
        let changed = was_active || state.entries.len() != previous_entry_count;
        drop(state);
        if changed {
            self.signal_activity();
        }
    }

    pub(super) fn admit_inputs(
        &self,
        cell_id: &CellId,
        inputs: impl IntoIterator<Item = DeferredCellInput>,
    ) -> Option<AdmittedCellInputBatch> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.active_cells.contains(cell_id) {
            return None;
        }
        let mut admitted = Vec::new();
        for input in inputs {
            let Some(input) = state.try_admit_input(cell_id, input) else {
                continue;
            };
            admitted.push(input);
        }
        (!admitted.is_empty()).then(|| AdmittedCellInputBatch {
            cell_id: cell_id.clone(),
            inputs: admitted,
            state: Arc::downgrade(&self.state),
        })
    }

    fn signal_activity(&self) {
        self.activity_tx
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

impl Default for DeferredCellInputRegistry {
    fn default() -> Self {
        let (activity_tx, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(DeferredCellInputState::default())),
            activity_tx,
        }
    }
}

impl AdmittedCellInputBatch {
    pub(crate) fn response_items(&self) -> Vec<ResponseItem> {
        self.inputs
            .iter()
            .map(DeferredCellInput::response_item)
            .collect()
    }
}

impl Drop for AdmittedCellInputBatch {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for input in &self.inputs {
            let item_id = input.item_id();
            let Some(outstanding) = state.outstanding.get_mut(item_id) else {
                continue;
            };
            if outstanding.cell_id != self.cell_id
                || outstanding.delivery != CellInputDelivery::InFlight
            {
                continue;
            }
            outstanding.delivery = CellInputDelivery::Delivered;
        }
    }
}

impl DeferredCellInputEntry {
    fn new(cell_id: CellId, input: DeferredCellInput) -> Self {
        let retained_bytes = cell_id
            .as_str()
            .len()
            .saturating_add(input.retained_bytes());
        Self {
            cell_id,
            input,
            retained_bytes,
        }
    }
}

impl DeferredCellInput {
    pub(super) fn notification(call_id: String, mut text: String) -> Self {
        truncate_text_to_bytes(&mut text, MAX_CODE_MODE_INPUT_TEXT_BYTES);
        let fixed_retained_bytes = call_id.len();
        Self {
            kind: DeferredCellInputKind::Notification,
            response_item: ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            },
            fixed_retained_bytes,
        }
    }

    pub(super) fn hook_context(mut text: String) -> Self {
        truncate_text_to_bytes(&mut text, MAX_CODE_MODE_INPUT_TEXT_BYTES);
        Self {
            kind: DeferredCellInputKind::HookContext,
            response_item: ContextualUserFragment::into(HookAdditionalContext::new(text)),
            fixed_retained_bytes: 0,
        }
    }

    pub(super) fn with_prepared_response_item(
        mut self,
        response_item: ResponseItem,
    ) -> Option<Self> {
        let prepared_id_is_valid = response_item.id().is_some_and(|id| !id.is_empty());
        let prepared_turn_id_is_valid = response_item.turn_id().is_some_and(|id| !id.is_empty());
        let prepared_text_matches = Self::text_for(self.kind, &response_item) == Some(self.text());
        let notification_call_id_matches = match (&self.response_item, &response_item) {
            (
                ResponseItem::CustomToolCallOutput {
                    call_id: expected, ..
                },
                ResponseItem::CustomToolCallOutput { call_id, .. },
            ) => expected == call_id,
            (ResponseItem::Message { .. }, ResponseItem::Message { .. }) => true,
            _ => false,
        };
        if !prepared_id_is_valid
            || !prepared_turn_id_is_valid
            || !prepared_text_matches
            || !notification_call_id_matches
        {
            return None;
        }
        self.response_item = response_item;
        Some(self)
    }

    fn retained_bytes(&self) -> usize {
        self.fixed_retained_bytes.saturating_add(self.text().len())
    }

    pub(super) fn response_item(&self) -> ResponseItem {
        self.response_item.clone()
    }

    fn model_visible_tokens(&self) -> usize {
        usize::try_from(crate::context_manager::estimate_item_token_count(
            &self.response_item,
        ))
        .unwrap_or(usize::MAX)
    }

    fn model_visible_token_upper_bound(&self) -> usize {
        usize::try_from(crate::context_manager::item_token_upper_bound(
            &self.response_item,
        ))
        .unwrap_or(usize::MAX)
    }

    fn item_id(&self) -> &ResponseItemId {
        let Some(item_id) = self.response_item.id() else {
            unreachable!("prepared deferred code-mode inputs always have an item ID");
        };
        item_id
    }

    fn turn_id(&self) -> &str {
        let Some(turn_id) = self.response_item.turn_id() else {
            unreachable!("prepared deferred code-mode inputs always have a turn ID");
        };
        turn_id
    }

    fn text(&self) -> &str {
        let Some(text) = Self::text_for(self.kind, &self.response_item) else {
            unreachable!("deferred code-mode input kind matches its response item");
        };
        text
    }

    fn text_for(kind: DeferredCellInputKind, item: &ResponseItem) -> Option<&str> {
        match (kind, item) {
            (
                DeferredCellInputKind::Notification,
                ResponseItem::CustomToolCallOutput { output, .. },
            ) => match &output.body {
                FunctionCallOutputBody::Text(text) => Some(text),
                FunctionCallOutputBody::ContentItems(_) => None,
            },
            (DeferredCellInputKind::HookContext, ResponseItem::Message { content, .. }) => {
                match content.as_slice() {
                    [ContentItem::InputText { text }] => Some(text),
                    _ => None,
                }
            }
            (DeferredCellInputKind::Notification, _) | (DeferredCellInputKind::HookContext, _) => {
                None
            }
        }
    }

    fn set_text(&mut self, text: String) {
        match (self.kind, &mut self.response_item) {
            (
                DeferredCellInputKind::Notification,
                ResponseItem::CustomToolCallOutput { output, .. },
            ) => output.body = FunctionCallOutputBody::Text(text),
            (DeferredCellInputKind::HookContext, ResponseItem::Message { content, .. }) => {
                match content.as_mut_slice() {
                    [ContentItem::InputText { text: current }] => *current = text,
                    _ => unreachable!("hook context contains exactly one text item"),
                }
            }
            (DeferredCellInputKind::Notification, _) | (DeferredCellInputKind::HookContext, _) => {
                unreachable!("deferred code-mode input kind matches its response item")
            }
        }
    }
}

impl DeferredCellInputState {
    fn try_admit_input(
        &mut self,
        cell_id: &CellId,
        input: DeferredCellInput,
    ) -> Option<DeferredCellInput> {
        let quota = self.admitted;
        if quota.items >= MAX_CODE_MODE_INPUT_ITEMS {
            return None;
        }
        let item_token_limit = MAX_CODE_MODE_INPUT_TOTAL_TOKENS
            .saturating_sub(quota.tokens)
            .min(MAX_CODE_MODE_INPUT_ITEM_TOKENS);
        let item_token_upper_bound_limit = MAX_CODE_MODE_INPUT_TOTAL_TOKENS
            .saturating_sub(quota.upper_bound_tokens)
            .min(MAX_CODE_MODE_INPUT_ITEM_TOKENS);
        if item_token_limit == 0 || item_token_upper_bound_limit == 0 {
            return None;
        }
        if self.outstanding.contains_key(input.item_id()) {
            return None;
        }

        let source_text = input.text().to_string();
        let candidate_at = |max_text_bytes| {
            let mut candidate = input.clone();
            let mut text = source_text.clone();
            truncate_text_to_bytes(&mut text, max_text_bytes);
            candidate.set_text(text);
            candidate
        };

        let fits = |candidate: &DeferredCellInput| {
            candidate.model_visible_tokens() <= item_token_limit
                && candidate.model_visible_token_upper_bound() <= item_token_upper_bound_limit
        };
        let mut admitted_input = candidate_at(MAX_CODE_MODE_INPUT_TEXT_BYTES);
        if !fits(&admitted_input) {
            let mut lower = 0_usize;
            let mut upper = source_text.len().min(MAX_CODE_MODE_INPUT_TEXT_BYTES);
            while lower < upper {
                let midpoint = lower + (upper - lower).div_ceil(2);
                if fits(&candidate_at(midpoint)) {
                    lower = midpoint;
                } else {
                    upper = midpoint.saturating_sub(1);
                }
            }
            admitted_input = candidate_at(lower);
        }
        let admitted_tokens = admitted_input.model_visible_tokens();
        let admitted_upper_bound_tokens = admitted_input.model_visible_token_upper_bound();
        if admitted_tokens > item_token_limit
            || admitted_upper_bound_tokens > item_token_upper_bound_limit
        {
            return None;
        }
        self.admitted = CellInputQuota {
            items: quota.items.saturating_add(1),
            tokens: quota.tokens.saturating_add(admitted_tokens),
            upper_bound_tokens: quota
                .upper_bound_tokens
                .saturating_add(admitted_upper_bound_tokens),
        };
        self.outstanding.insert(
            admitted_input.item_id().clone(),
            OutstandingCellInput {
                cell_id: cell_id.clone(),
                tokens: admitted_tokens,
                upper_bound_tokens: admitted_upper_bound_tokens,
                turn_id: admitted_input.turn_id().to_string(),
                delivery: CellInputDelivery::InFlight,
            },
        );
        Some(admitted_input)
    }

    fn release_admission(&mut self, item_id: &ResponseItemId) {
        let Some(outstanding) = self.outstanding.remove(item_id) else {
            return;
        };
        self.admitted.items = self.admitted.items.saturating_sub(1);
        self.admitted.tokens = self.admitted.tokens.saturating_sub(outstanding.tokens);
        self.admitted.upper_bound_tokens = self
            .admitted
            .upper_bound_tokens
            .saturating_sub(outstanding.upper_bound_tokens);
    }

    fn enforce_bounds_from_front(&mut self) {
        while self.entries.len() > MAX_CODE_MODE_INPUT_ITEMS
            || self.retained_bytes > MAX_CODE_MODE_INPUT_TOTAL_BYTES
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
            self.release_admission(entry.input.item_id());
        }
    }

    fn enforce_bounds_from_back(&mut self) {
        while self.entries.len() > MAX_CODE_MODE_INPUT_ITEMS
            || self.retained_bytes > MAX_CODE_MODE_INPUT_TOTAL_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
            self.release_admission(entry.input.item_id());
        }
    }
}

fn truncate_text_to_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }

    let marker = if max_bytes >= TRUNCATION_MARKER.len() {
        TRUNCATION_MARKER
    } else {
        ""
    };
    let mut retained_bytes = max_bytes.saturating_sub(marker.len());
    while !text.is_char_boundary(retained_bytes) {
        retained_bytes = retained_bytes.saturating_sub(1);
    }
    text.truncate(retained_bytes);
    text.push_str(marker);
}

#[cfg(test)]
#[path = "deferred_inputs_tests.rs"]
mod tests;
