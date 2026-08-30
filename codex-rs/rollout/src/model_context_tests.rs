use super::MAX_PRESERVED_DURABLE_SCAN_ITEMS;
use super::MAX_PRESERVED_DURABLE_STATE_BYTES;
use super::MAX_PRESERVED_DURABLE_STATE_ITEMS;
use super::ModelContextScan;
use super::ModelContextScanMode;
use super::ModelContextScanProgress;
use crate::RolloutItem;

fn model_invisible_mail_metadata() -> RolloutItem {
    RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
        mail_fingerprint: None,
        delivery_id: None,
        terminal_turn_id: None,
        terminal_status: None,
        mail_communication: None,
        mail_start_options: None,
        consumed_mail_id: None,
    }
}

#[test]
fn durable_scan_fails_closed_at_its_hard_item_limit() {
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    for _ in 0..MAX_PRESERVED_DURABLE_SCAN_ITEMS {
        assert_eq!(
            scan.push(model_invisible_mail_metadata()),
            ModelContextScanProgress::Continue
        );
    }

    assert_eq!(
        scan.push(model_invisible_mail_metadata()),
        ModelContextScanProgress::LimitExceeded
    );
}

#[test]
fn durable_scan_discards_irrelevant_history_after_the_context_cutoff() {
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);
    for _ in 0..50_000 {
        assert_eq!(
            scan.push(model_invisible_mail_metadata()),
            ModelContextScanProgress::Continue
        );
    }

    assert!(scan.items_newest_first.is_empty());
    assert!(scan.older_durable_items_newest_first.is_empty());
}

#[test]
fn durable_scan_fails_closed_when_retained_state_exceeds_its_bound() {
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);
    let durable_metadata = || RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
        mail_fingerprint: Some("fingerprint".to_string()),
        delivery_id: None,
        terminal_turn_id: None,
        terminal_status: None,
        mail_communication: None,
        mail_start_options: None,
        consumed_mail_id: None,
    };
    for _ in 0..MAX_PRESERVED_DURABLE_STATE_ITEMS {
        assert_eq!(
            scan.push(durable_metadata()),
            ModelContextScanProgress::Continue
        );
    }

    assert_eq!(
        scan.push(durable_metadata()),
        ModelContextScanProgress::LimitExceeded
    );
}

#[test]
fn durable_scan_fails_closed_when_retained_state_exceeds_its_byte_bound() {
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);
    scan.retained_durable_state_bytes = MAX_PRESERVED_DURABLE_STATE_BYTES;

    assert_eq!(
        scan.push(RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: None,
        }),
        ModelContextScanProgress::LimitExceeded
    );
}
