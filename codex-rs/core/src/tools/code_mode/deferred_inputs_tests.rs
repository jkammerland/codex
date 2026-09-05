use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_code_mode::CellId;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::AdmittedCellInputBatch;
use super::DeferredCellInput;
use super::DeferredCellInputRegistry;
use super::MAX_CODE_MODE_INPUT_ITEM_TOKENS;
use super::MAX_CODE_MODE_INPUT_ITEMS;
use super::MAX_CODE_MODE_INPUT_TEXT_BYTES;
use super::MAX_CODE_MODE_INPUT_TOTAL_BYTES;
use super::MAX_CODE_MODE_INPUT_TOTAL_TOKENS;
use super::TRUNCATION_MARKER;

const TEST_TURN_ID: &str = "019a1016-983a-7a00-bbf1-93131d17739c";
static NEXT_ITEM_ID: AtomicU64 = AtomicU64::new(0);

fn prepare_input(input: DeferredCellInput, turn_id: &str) -> DeferredCellInput {
    let mut item = input.response_item();
    item.set_turn_id_if_missing(turn_id);
    item.set_create_time_if_missing(serde_json::Number::from(1_788_454_400));
    let prefix = item
        .id_prefix()
        .expect("deferred test input should support response item IDs");
    item.set_id(Some(ResponseItemId::with_suffix(
        prefix,
        NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed),
    )));
    input
        .with_prepared_response_item(item)
        .expect("test input should remain structurally valid after preparation")
}

fn admit_hook_contexts(
    registry: &DeferredCellInputRegistry,
    cell_id: &CellId,
    contexts: Vec<String>,
) -> Option<AdmittedCellInputBatch> {
    registry.open_cell(cell_id);
    registry.admit_inputs(
        cell_id,
        contexts
            .into_iter()
            .map(DeferredCellInput::hook_context)
            .map(|input| prepare_input(input, TEST_TURN_ID)),
    )
}

fn admit_notification(
    registry: &DeferredCellInputRegistry,
    cell_id: &CellId,
    call_id: &str,
    text: String,
) -> Option<AdmittedCellInputBatch> {
    registry.open_cell(cell_id);
    registry.admit_inputs(
        cell_id,
        std::iter::once(prepare_input(
            DeferredCellInput::notification(call_id.to_string(), text),
            TEST_TURN_ID,
        )),
    )
}

fn defer_hook_contexts(
    registry: &DeferredCellInputRegistry,
    cell_id: &CellId,
    contexts: Vec<String>,
) -> bool {
    let Some(batch) = admit_hook_contexts(registry, cell_id, contexts) else {
        return false;
    };
    registry.defer_admitted(batch);
    true
}

fn defer_notification(
    registry: &DeferredCellInputRegistry,
    cell_id: &CellId,
    call_id: &str,
    text: String,
) {
    let batch = admit_notification(registry, cell_id, call_id, text)
        .expect("test notification should be admitted");
    registry.defer_admitted(batch);
}

fn batch_tokens(batch: &AdmittedCellInputBatch) -> usize {
    batch
        .inputs
        .iter()
        .map(DeferredCellInput::model_visible_tokens)
        .sum()
}

fn batch_token_upper_bound(batch: &AdmittedCellInputBatch) -> usize {
    batch
        .inputs
        .iter()
        .map(DeferredCellInput::model_visible_token_upper_bound)
        .sum()
}

#[test]
fn deferred_inputs_are_bound_to_their_cell_and_consumed_once() {
    let registry = DeferredCellInputRegistry::default();
    let first_cell = CellId::new("first".to_string());
    let second_cell = CellId::new("second".to_string());
    assert!(defer_hook_contexts(
        &registry,
        &first_cell,
        vec!["pre context".to_string(), "post context".to_string()],
    ));
    defer_notification(
        &registry,
        &second_cell,
        "call-2",
        "notification".to_string(),
    );

    let first_items = registry
        .take_admitted(&first_cell)
        .expect("first cell should have deferred input")
        .response_items();
    let first_texts = first_items
        .iter()
        .map(|item| match item {
            ResponseItem::Message { content, .. } => match content.as_slice() {
                [ContentItem::InputText { text }] => text.as_str(),
                _ => panic!("hook context should contain exactly one text item"),
            },
            _ => panic!("first cell should contain only hook contexts"),
        })
        .collect::<Vec<_>>();
    assert_eq!(first_texts, vec!["pre context", "post context"]);
    assert!(first_items.iter().all(|item| {
        item.id().is_some_and(|id| !id.is_empty()) && item.turn_id() == Some(TEST_TURN_ID)
    }));
    assert!(registry.take_admitted(&first_cell).is_none());

    let second_items = registry
        .take_admitted(&second_cell)
        .expect("second cell should have deferred input")
        .response_items();
    assert_eq!(second_items.len(), 1);
    assert!(matches!(
        &second_items[0],
        ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == "call-2"
    ));
}

#[test]
fn deferred_input_storage_is_bounded_and_unicode_safe() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("bounded".to_string());
    for _ in 0..=MAX_CODE_MODE_INPUT_ITEMS {
        if !defer_hook_contexts(
            &registry,
            &cell_id,
            vec!["🦀".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES)],
        ) {
            break;
        }
    }

    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(state.entries.len() <= MAX_CODE_MODE_INPUT_ITEMS);
    assert!(state.retained_bytes <= MAX_CODE_MODE_INPUT_TOTAL_BYTES);
    assert!(state.entries.iter().all(|entry| {
        let text = entry.input.text();
        text.len() <= MAX_CODE_MODE_INPUT_TEXT_BYTES && text.ends_with(TRUNCATION_MARKER)
    }));
    assert_eq!(state.outstanding.len(), state.entries.len());
    drop(state);

    let count_bounded_registry = DeferredCellInputRegistry::default();
    for index in 0..=MAX_CODE_MODE_INPUT_ITEMS {
        if !defer_hook_contexts(&count_bounded_registry, &cell_id, vec![index.to_string()]) {
            break;
        }
    }
    let state = count_bounded_registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let retained_texts = state
        .entries
        .iter()
        .map(|entry| entry.input.text().to_string())
        .collect::<Vec<_>>();
    assert!(retained_texts.len() <= MAX_CODE_MODE_INPUT_ITEMS);
    assert_eq!(
        retained_texts,
        (0..state.entries.len())
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn admission_truncation_handles_exact_and_multibyte_boundaries() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("truncation".to_string());
    let at_limit = "a".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES);
    let at_limit_batch = admit_hook_contexts(&registry, &cell_id, vec![at_limit])
        .expect("at-limit context should be admitted");
    assert!(at_limit_batch.inputs[0].model_visible_tokens() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS);
    assert!(
        at_limit_batch.inputs[0].model_visible_token_upper_bound()
            <= MAX_CODE_MODE_INPUT_ITEM_TOKENS
    );
    assert!(matches!(
        at_limit_batch.response_items().as_slice(),
        [ResponseItem::Message { content, .. }]
            if matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text.len() <= MAX_CODE_MODE_INPUT_TEXT_BYTES
                    && text.ends_with(TRUNCATION_MARKER))
    ));

    registry.acknowledge_model_sample(&at_limit_batch.response_items(), TEST_TURN_ID);
    let multibyte_batch = admit_hook_contexts(
        &registry,
        &cell_id,
        vec!["🦀".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES)],
    )
    .expect("oversized multibyte context should be admitted");
    assert!(multibyte_batch.inputs[0].model_visible_tokens() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS);
    assert!(
        multibyte_batch.inputs[0].model_visible_token_upper_bound()
            <= MAX_CODE_MODE_INPUT_ITEM_TOKENS
    );
    assert!(matches!(
        multibyte_batch.response_items().as_slice(),
        [ResponseItem::Message { content, .. }]
            if matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text.is_char_boundary(text.len())
                    && text.len() <= MAX_CODE_MODE_INPUT_TEXT_BYTES
                    && text.ends_with(TRUNCATION_MARKER))
    ));
}

#[test]
fn admission_hard_cap_covers_token_dense_punctuation_and_unicode() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("token-dense".to_string());
    for (index, text) in [
        "!@#$%^&*()[]{};:,./<>?|\\\"'~".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
        "🦀⚙️界e\u{301}".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
    ]
    .into_iter()
    .enumerate()
    {
        let batch = admit_notification(
            &registry,
            &cell_id,
            &format!("token-dense-{index}"),
            text,
        )
        .expect("a truncated token-dense input should fit its envelope");
        assert!(batch.inputs[0].model_visible_tokens() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS);
        assert!(
            batch.inputs[0].model_visible_token_upper_bound()
                <= MAX_CODE_MODE_INPUT_ITEM_TOKENS
        );
        let sampled = batch.response_items();
        registry.acknowledge_model_sample(&sampled, TEST_TURN_ID);
    }
}

#[test]
fn admission_charges_final_stamped_items_and_json_escaping() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("escaped".to_string());
    registry.open_cell(&cell_id);
    let escaped = "\0".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES);
    let mut admitted = Vec::new();
    for index in 0..MAX_CODE_MODE_INPUT_ITEMS {
        let input = DeferredCellInput::notification(format!("escaped-{index}"), escaped.clone());
        let input = prepare_input(input, TEST_TURN_ID);
        let Some(batch) = registry.admit_inputs(&cell_id, std::iter::once(input)) else {
            break;
        };
        let response_items = batch.response_items();
        let item = &response_items[0];
        assert!(batch.inputs[0].model_visible_tokens() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS);
        assert!(
            batch.inputs[0].model_visible_token_upper_bound()
                <= MAX_CODE_MODE_INPUT_ITEM_TOKENS
        );
        assert!(item.id().is_some_and(|id| !id.is_empty()));
        assert_eq!(item.turn_id(), Some(TEST_TURN_ID));
        assert!(
            serde_json::to_value(item)
                .expect("prepared input should serialize")
                .pointer("/internal_chat_message_metadata_passthrough/create_time")
                .is_some()
        );
        admitted.push(batch);
    }

    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        state.admitted.tokens,
        admitted.iter().map(batch_tokens).sum::<usize>()
    );
    assert_eq!(
        state.admitted.upper_bound_tokens,
        admitted
            .iter()
            .map(batch_token_upper_bound)
            .sum::<usize>()
    );
    assert_eq!(
        state.outstanding.len(),
        admitted
            .iter()
            .map(|batch| batch.inputs.len())
            .sum::<usize>()
    );
    assert!(state.admitted.tokens <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    assert!(state.admitted.upper_bound_tokens <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    assert!(admitted.len() < MAX_CODE_MODE_INPUT_ITEMS);
}

#[test]
fn snapshot_acknowledgement_does_not_release_inputs_admitted_after_capture() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("snapshot-race".to_string());
    let captured_prompt = Vec::new();
    let raced_batch = admit_notification(
        &registry,
        &cell_id,
        "after-snapshot",
        "x".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
    )
    .expect("input admitted after prompt capture should be retained");

    registry.acknowledge_model_sample(&captured_prompt, TEST_TURN_ID);
    let mut later_batches = vec![raced_batch];
    for index in 0..MAX_CODE_MODE_INPUT_ITEMS {
        let Some(batch) = admit_notification(
            &registry,
            &cell_id,
            &format!("next-generation-{index}"),
            "y".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
        ) else {
            break;
        };
        later_batches.push(batch);
    }

    let later_prompt = later_batches
        .iter()
        .flat_map(AdmittedCellInputBatch::response_items)
        .collect::<Vec<_>>();
    let later_prompt_tokens = later_prompt
        .iter()
        .map(crate::context_manager::estimate_item_token_count)
        .map(|tokens| usize::try_from(tokens).unwrap_or(usize::MAX))
        .sum::<usize>();
    let later_prompt_token_upper_bound = later_prompt
        .iter()
        .map(crate::context_manager::item_token_upper_bound)
        .map(|tokens| usize::try_from(tokens).unwrap_or(usize::MAX))
        .sum::<usize>();
    assert!(later_prompt_tokens <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    assert!(later_prompt_token_upper_bound <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.admitted.tokens, later_prompt_tokens);
    assert_eq!(
        state.admitted.upper_bound_tokens,
        later_prompt_token_upper_bound
    );
    assert_eq!(state.outstanding.len(), later_prompt.len());
}

#[test]
fn all_cells_share_quota_until_exact_sampled_ids_are_acknowledged() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("shared-quota".to_string());
    let deferred_contexts = (0..40)
        .map(|index| format!("deferred-{index}:{}", "x".repeat(300)))
        .collect();
    assert!(defer_hook_contexts(&registry, &cell_id, deferred_contexts));

    let deferred = registry
        .take_admitted(&cell_id)
        .expect("deferred contexts should remain admitted until sampled");
    let deferred_count = deferred.inputs.len();
    let mut active = Vec::new();
    for index in 0..MAX_CODE_MODE_INPUT_ITEMS {
        let active_cell_id = CellId::new(format!("active-cell-{index}"));
        let Some(batch) = admit_notification(
            &registry,
            &active_cell_id,
            &format!("notify-{index}"),
            "y".repeat(300),
        ) else {
            break;
        };
        active.push(batch);
    }

    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let quota = state.admitted;
    assert_eq!(quota.items, deferred_count + active.len());
    assert!(quota.items <= MAX_CODE_MODE_INPUT_ITEMS);
    assert!(quota.tokens <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    assert!(quota.upper_bound_tokens <= MAX_CODE_MODE_INPUT_TOTAL_TOKENS);
    drop(state);
    assert!(
        admit_notification(
            &registry,
            &cell_id,
            "exhausted",
            "z".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
        )
        .is_none(),
        "the shared aggregate quota should reject further active input"
    );

    let sampled = deferred.response_items();
    registry.acknowledge_model_sample(&sampled, TEST_TURN_ID);
    let reset = admit_notification(
        &registry,
        &cell_id,
        "after-ack",
        "🦀".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES),
    )
    .expect("acknowledging sampled IDs should release their exact quota");
    assert!(reset.inputs[0].model_visible_tokens() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS);
    assert!(
        reset.inputs[0].model_visible_token_upper_bound() <= MAX_CODE_MODE_INPUT_ITEM_TOKENS
    );
    assert!(matches!(
        reset.response_items().as_slice(),
        [ResponseItem::CustomToolCallOutput { output, .. }]
            if output.text_content().is_some_and(|text| {
                text.len() <= MAX_CODE_MODE_INPUT_TEXT_BYTES
                    && text.ends_with(TRUNCATION_MARKER)
            })
    ));
}

#[tokio::test]
async fn activity_subscription_observes_pending_and_future_cell_inputs_without_a_race() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("active".to_string());
    defer_notification(
        &registry,
        &cell_id,
        "already-pending",
        "notification".to_string(),
    );

    let _activity_rx = registry.subscribe_activity();
    assert!(
        registry.has_pending_for_cell(&cell_id),
        "subscribing after a push must still observe the retained input"
    );
    let sampled = registry
        .take_admitted(&cell_id)
        .expect("pending input should remain admitted")
        .response_items();
    registry.acknowledge_model_sample(&sampled, TEST_TURN_ID);

    let mut activity_rx = registry.subscribe_activity();
    assert!(defer_hook_contexts(
        &registry,
        &cell_id,
        vec!["hook context".to_string()]
    ));
    activity_rx
        .changed()
        .await
        .expect("hook context should signal deferred activity");
    assert!(registry.has_pending_for_cell(&cell_id));
}

#[test]
fn new_turn_releases_old_delivered_inputs_but_retains_deferred_inputs() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("aborted-turn".to_string());
    let delivered = admit_notification(
        &registry,
        &cell_id,
        "delivered-before-abort",
        "delivered".to_string(),
    )
    .expect("delivered input should be admitted");
    assert!(defer_hook_contexts(
        &registry,
        &cell_id,
        vec!["never delivered".to_string()]
    ));
    let delivered_items = delivered.response_items();
    drop(delivered);
    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.outstanding.len(), delivered_items.len() + 1);
    assert_eq!(state.entries.len(), 1);
    drop(state);

    registry.acknowledge_model_sample(&[], "next-turn");
    {
        let state = registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.admitted.items, 1);
        assert_eq!(state.outstanding.len(), 1);
        assert_eq!(state.entries.len(), 1);
        assert!(state.retained_bytes > 0);
    }

    let deferred_items = registry
        .take_admitted(&cell_id)
        .expect("deferred input should remain recoverable across turns")
        .response_items();
    registry.acknowledge_model_sample(&deferred_items, "next-turn");
    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.admitted.items, 0);
    assert_eq!(state.admitted.tokens, 0);
    assert_eq!(state.admitted.upper_bound_tokens, 0);
    assert_eq!(state.outstanding.len(), 0);
    assert_eq!(state.entries.len(), 0);
    assert_eq!(state.retained_bytes, 0);
}

#[test]
fn new_turn_acknowledgement_does_not_release_an_in_flight_failed_delivery() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("in-flight-across-turns".to_string());
    let batch = admit_notification(
        &registry,
        &cell_id,
        "in-flight",
        "delivery still pending".to_string(),
    )
    .expect("in-flight notification should be admitted");

    registry.acknowledge_model_sample(&[], "next-turn");
    {
        let state = registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.admitted.items, 1);
        assert_eq!(state.outstanding.len(), 1);
        assert!(state.entries.is_empty());
    }

    registry.defer_admitted(batch);
    let deferred = registry
        .take_admitted(&cell_id)
        .expect("a failed in-flight delivery must remain deferred for the live cell");
    assert!(matches!(
        deferred.response_items().as_slice(),
        [ResponseItem::CustomToolCallOutput { output, .. }]
            if output.text_content() == Some("delivery still pending")
    ));
}

#[test]
fn delivered_input_stays_charged_and_cannot_be_readmitted_before_sampling() {
    let registry = DeferredCellInputRegistry::default();
    let cell_id = CellId::new("delivered-not-sampled".to_string());
    registry.open_cell(&cell_id);
    let input = prepare_input(
        DeferredCellInput::notification("delivered".to_string(), "already in history".to_string()),
        TEST_TURN_ID,
    );
    let delivered = registry
        .admit_inputs(&cell_id, std::iter::once(input.clone()))
        .expect("delivered notification should be admitted");
    let delivered_items = delivered.response_items();
    drop(delivered);

    {
        let state = registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.admitted.items, 1);
        assert!(
            state
                .outstanding
                .values()
                .all(|outstanding| { outstanding.delivery == super::CellInputDelivery::Delivered })
        );
    }
    assert!(
        registry
            .admit_inputs(&cell_id, std::iter::once(input))
            .is_none(),
        "an unsampled delivered item ID must remain charged"
    );

    registry.acknowledge_model_sample(&delivered_items, TEST_TURN_ID);
    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.admitted.items, 0);
    assert!(state.outstanding.is_empty());
}

#[test]
fn terminal_cell_releases_deferred_quota_and_preserves_one_shot_delivery() {
    let registry = DeferredCellInputRegistry::default();
    let abandoned_cell = CellId::new("abandoned".to_string());
    let live_cell = CellId::new("live".to_string());
    let abandoned_delivered = admit_notification(
        &registry,
        &abandoned_cell,
        "abandoned-delivered",
        "delivered before closure".to_string(),
    )
    .expect("delivered abandoned-cell input should be admitted");
    assert!(defer_hook_contexts(
        &registry,
        &abandoned_cell,
        vec!["abandoned deferred context".to_string()]
    ));
    assert!(defer_hook_contexts(
        &registry,
        &live_cell,
        vec!["live deferred context".to_string()]
    ));

    registry.discard_cell(&abandoned_cell);
    registry.defer_admitted(abandoned_delivered);
    let late_input = prepare_input(
        DeferredCellInput::notification(
            "late-after-close".to_string(),
            "must be rejected".to_string(),
        ),
        TEST_TURN_ID,
    );
    assert!(
        registry
            .admit_inputs(&abandoned_cell, std::iter::once(late_input))
            .is_none(),
        "callbacks racing after terminal closure must not consume quota"
    );

    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.admitted.items, 1);
    assert_eq!(state.outstanding.len(), 1);
    assert_eq!(state.entries.len(), 1);
    assert_eq!(state.terminal_entries.len(), 1);
    assert!(state.entries.iter().all(|entry| entry.cell_id == live_cell));
    assert!(
        state
            .outstanding
            .values()
            .all(|outstanding| outstanding.cell_id == live_cell)
    );
    drop(state);

    let terminal_items = registry
        .take_admitted(&abandoned_cell)
        .expect("a terminal observation should get one final delivery attempt")
        .response_items();
    assert!(matches!(
        terminal_items.as_slice(),
        [ResponseItem::Message { content, .. }]
            if matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text == "abandoned deferred context")
    ));
    assert!(
        registry.take_admitted(&abandoned_cell).is_none(),
        "terminal delivery must be one-shot"
    );
    let live_items = registry
        .take_admitted(&live_cell)
        .expect("closing another cell must preserve live-cell delivery")
        .response_items();
    let sampled_items = terminal_items
        .into_iter()
        .chain(live_items)
        .collect::<Vec<_>>();
    registry.acknowledge_model_sample(&sampled_items, TEST_TURN_ID);
    assert!(
        admit_notification(
            &registry,
            &CellId::new("replacement".to_string()),
            "replacement",
            "released quota is reusable".to_string(),
        )
        .is_some(),
        "terminal cleanup must make abandoned-cell quota reusable"
    );
}

#[test]
fn abandoned_terminal_delivery_cache_is_bounded_without_consuming_input_quota() {
    let registry = DeferredCellInputRegistry::default();
    for index in 0..=MAX_CODE_MODE_INPUT_ITEMS {
        let cell_id = CellId::new(format!("abandoned-{index}"));
        assert!(defer_hook_contexts(
            &registry,
            &cell_id,
            vec!["x".repeat(MAX_CODE_MODE_INPUT_TEXT_BYTES)]
        ));
        registry.discard_cell(&cell_id);
    }

    let state = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.admitted.items, 0);
    assert_eq!(state.admitted.tokens, 0);
    assert_eq!(state.admitted.upper_bound_tokens, 0);
    assert!(state.outstanding.is_empty());
    assert!(state.entries.is_empty());
    assert!(state.terminal_entries.len() <= MAX_CODE_MODE_INPUT_ITEMS);
    assert!(state.terminal_retained_bytes <= MAX_CODE_MODE_INPUT_TOTAL_BYTES);
}
