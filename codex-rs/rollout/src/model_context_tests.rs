use super::MAX_PRESERVED_DURABLE_SCAN_ITEMS;
use super::MAX_PRESERVED_DURABLE_STATE_BYTES;
use super::MAX_PRESERVED_DURABLE_STATE_ITEMS;
use super::ModelContextScan;
use super::ModelContextScanMode;
use super::ModelContextScanProgress;
use super::compact_durable_agent_state;
use crate::CompactedItem;
use crate::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;

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

fn legacy_claim(id: &ResponseItemId, turn_id: &str) -> RolloutItem {
    durable_claim(id, "fingerprint", turn_id, /*delivered*/ false)
}

fn durable_claim(
    id: &ResponseItemId,
    fingerprint: &str,
    turn_id: &str,
    delivered: bool,
) -> RolloutItem {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "legacy claim body".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(id.clone());
    RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: id.clone(),
        communication: Some(communication.clone()),
        delivered_response: delivered.then(|| communication.to_model_input_item().into()),
        fingerprint: fingerprint.to_string(),
        turn_id: turn_id.to_string(),
    }
}

fn durable_payload(id: &ResponseItemId, fingerprint: &str, content: &str) -> RolloutItem {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        content.to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(id.clone());
    RolloutItem::DurableInterAgentCommunication {
        communication,
        start_options: Default::default(),
        fingerprint: fingerprint.to_string(),
        turn_id: "mail-turn".to_string(),
        compatibility_shadow: false,
    }
}

fn assert_rollout_items_eq(actual: &[RolloutItem], expected: &[RolloutItem]) {
    assert_eq!(
        serde_json::to_value(actual).expect("serialize actual rollout items"),
        serde_json::to_value(expected).expect("serialize expected rollout items")
    );
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn finish(scan: ModelContextScan) -> Vec<RolloutItem> {
    scan.finish(SessionMetaLine {
        meta: SessionMeta::default(),
        git: None,
    })
}

fn completed_turn_context(turn_id: &str) -> RolloutItem {
    RolloutItem::TurnContext(TurnContextItem {
        turn_id: Some(turn_id.to_string()),
        cwd: serde_json::from_value(serde_json::json!("/tmp")).expect("absolute cwd"),
        workspace_roots: None,
        current_date: None,
        timezone: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: None,
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "test-model".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: None,
        cyber_access_program: None,
        effort: None,
        summary: ReasoningSummary::Auto,
    })
}

fn agent_response() -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: None,
            author: "assistant".to_string(),
            recipient: "user".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "done".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
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

#[test]
fn durable_scan_does_not_apply_a_prior_turn_start_to_a_later_legacy_claim() {
    let id = ResponseItemId::with_suffix("msg", "started-before-claim");
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);

    assert_eq!(
        scan.push(legacy_claim(&id, "claimed-turn")),
        ModelContextScanProgress::Continue
    );
    assert_eq!(
        scan.push(turn_started("claimed-turn")),
        ModelContextScanProgress::Continue
    );

    let items = finish(scan);
    assert!(items.iter().any(|item| matches!(
        item,
        RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            ..
        } if communication_id == &id
    )));
    assert!(!items.iter().any(|item| matches!(
        item,
        RolloutItem::InterAgentCommunicationMetadata {
            consumed_mail_id: Some(consumed_id),
            ..
        } if consumed_id == &id
    )));
}

#[test]
fn durable_scan_applies_a_later_turn_start_to_an_earlier_claim() {
    let id = ResponseItemId::with_suffix("msg", "future-start");
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);

    assert_eq!(
        scan.push(turn_started("reused-turn")),
        ModelContextScanProgress::Continue
    );
    assert_eq!(
        scan.push(legacy_claim(&id, "reused-turn")),
        ModelContextScanProgress::Continue
    );

    let items = finish(scan);
    assert!(items.iter().any(|item| matches!(
        item,
        RolloutItem::InterAgentCommunicationMetadata {
            consumed_mail_id: Some(consumed_id),
            ..
        } if consumed_id == &id
    )));
    assert!(!items.iter().any(|item| matches!(
        item,
        RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            ..
        } if communication_id == &id
    )));
}

#[test]
fn production_cutoff_does_not_apply_a_prior_start_to_a_later_claim() {
    let id = ResponseItemId::with_suffix("msg", "cutoff-split");
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);

    for item in [
        agent_response(),
        completed_turn_context("newest-turn"),
        turn_started("newest-turn"),
        legacy_claim(&id, "legacy-turn"),
        RolloutItem::Compacted(CompactedItem {
            message: "checkpoint".to_string(),
            replacement_history: Some(Vec::new()),
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        }),
    ] {
        assert_eq!(scan.push(item), ModelContextScanProgress::Continue);
    }
    assert!(scan.has_bounded_suffix());
    assert_eq!(
        scan.push(turn_started("legacy-turn")),
        ModelContextScanProgress::Continue
    );

    let items = finish(scan);
    assert!(!items.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::TurnStarted(started))
            if started.turn_id == "legacy-turn"
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            ..
        } if communication_id == &id
    )));
}

#[test]
fn durable_scan_preserves_the_body_of_an_unstarted_legacy_claim() {
    let id = ResponseItemId::with_suffix("msg", "claim-only");
    let expected = legacy_claim(&id, "unstarted-turn");
    let RolloutItem::DurableInterAgentCommunicationClaimed {
        communication: expected_communication,
        ..
    } = &expected
    else {
        panic!("fixture should be a durable claim");
    };
    let mut scan = ModelContextScan::new(ModelContextScanMode::PreserveDurableAgentState);
    scan.bounded_suffix_len = Some(0);

    assert_eq!(
        scan.push(expected.clone()),
        ModelContextScanProgress::Continue
    );

    let retained_communication = finish(scan).into_iter().find_map(|item| match item {
        RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            communication,
            ..
        } if communication_id == id => communication,
        _ => None,
    });
    assert_eq!(
        retained_communication.as_ref(),
        expected_communication.as_ref()
    );
}

#[test]
fn durable_compaction_keeps_a_delivered_claim_consumed_after_a_pending_duplicate() {
    let id = ResponseItemId::with_suffix("msg", "delivered-before-pending");
    let compacted = compact_durable_agent_state(
        &[
            durable_payload(&id, "fingerprint", "mail body"),
            durable_claim(
                &id,
                "fingerprint",
                "delivered-turn",
                /*delivered*/ true,
            ),
        ],
        &[durable_claim(
            &id,
            "fingerprint",
            "pending-turn",
            /*delivered*/ false,
        )],
    );

    assert_rollout_items_eq(
        &compacted,
        &[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: Some(id),
        }],
    );
}

#[test]
fn durable_compaction_keeps_an_older_fingerprint_consumed_after_a_new_pending_payload() {
    let id = ResponseItemId::with_suffix("msg", "delivered-old-fingerprint");
    let compacted = compact_durable_agent_state(
        &[
            durable_payload(&id, "old-fingerprint", "old mail body"),
            durable_claim(
                &id,
                "old-fingerprint",
                "delivered-turn",
                /*delivered*/ true,
            ),
            durable_payload(&id, "new-fingerprint", "new mail body"),
            durable_claim(
                &id,
                "new-fingerprint",
                "pending-turn",
                /*delivered*/ false,
            ),
        ],
        &[],
    );

    assert_rollout_items_eq(
        &compacted,
        &[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("old-fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: Some(id),
        }],
    );
}

#[test]
fn durable_compaction_preserves_pending_occurrences_in_fifo_order() {
    let first_id = ResponseItemId::with_suffix("msg", "first");
    let second_id = ResponseItemId::with_suffix("msg", "second");
    let compacted = compact_durable_agent_state(
        &[
            durable_payload(&first_id, "first-old", "old first body"),
            durable_payload(&second_id, "second", "second body"),
            durable_payload(&first_id, "first-new", "new first body"),
        ],
        &[],
    );

    assert_rollout_items_eq(
        &compacted,
        &[
            durable_payload(&first_id, "first-old", "old first body"),
            durable_payload(&second_id, "second", "second body"),
            durable_payload(&first_id, "first-new", "new first body"),
        ],
    );
}

#[test]
fn durable_compaction_preserves_fifo_across_the_bounded_suffix() {
    let first_id = ResponseItemId::with_suffix("msg", "cutoff-first");
    let second_id = ResponseItemId::with_suffix("msg", "cutoff-second");
    let older = [
        durable_payload(&first_id, "first-old", "old first body"),
        durable_payload(&second_id, "second", "second body"),
    ];
    let suffix = [durable_payload(&first_id, "first-new", "new first body")];
    let mut reconstructed = compact_durable_agent_state(&older, &suffix);
    reconstructed.extend(suffix);

    assert_rollout_items_eq(
        &reconstructed,
        &[
            durable_payload(&first_id, "first-old", "old first body"),
            durable_payload(&second_id, "second", "second body"),
            durable_payload(&first_id, "first-new", "new first body"),
        ],
    );
}

#[test]
fn durable_compaction_keeps_mcp10_delivery_consumed_after_a_conflicting_payload() {
    let id = ResponseItemId::with_suffix("msg", "mcp10-conflict");
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "delivered body".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(id.clone());
    let compacted = compact_durable_agent_state(
        &[
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: true,
                mail_fingerprint: Some("delivered-fingerprint".to_string()),
                delivery_id: None,
                terminal_turn_id: None,
                terminal_status: None,
                mail_communication: None,
                mail_start_options: None,
                consumed_mail_id: None,
            },
            RolloutItem::ResponseItem(communication.to_model_input_item().into()),
            durable_payload(&id, "pending-fingerprint", "pending body"),
        ],
        &[],
    );

    assert_rollout_items_eq(
        &compacted,
        &[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("delivered-fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: Some(id),
        }],
    );
}

#[test]
fn durable_compaction_uses_a_suffix_delivery_to_suppress_an_older_payload() {
    let id = ResponseItemId::with_suffix("msg", "suffix-delivered");
    let compacted = compact_durable_agent_state(
        &[durable_payload(&id, "old-fingerprint", "old body")],
        &[durable_claim(
            &id,
            "delivered-fingerprint",
            "suffix-turn",
            /*delivered*/ true,
        )],
    );

    assert_rollout_items_eq(
        &compacted,
        &[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("delivered-fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: Some(id),
        }],
    );
}
