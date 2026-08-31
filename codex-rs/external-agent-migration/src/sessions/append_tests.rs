use super::super::ConversationMessage;
use super::super::MessageRole;
use super::super::export::EXTERNAL_SESSION_IMPORTED_MARKER;
use super::super::export::rollout_items_from_messages;
use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::security_risk::SecurityRiskScore;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn returns_the_missing_suffix_from_its_visible_boundary() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::Assistant, "late answer"),
        (MessageRole::User, "follow-up request"),
    ]);

    let suffix = plan_append(&source, &history).expect("exact prefix should append");

    assert!(matches!(
        suffix.first(),
        Some(RolloutItem::EventMsg(EventMsg::AgentMessage(event)))
            if event.message == "late answer"
    ));
    assert_eq!(
        model_messages(&suffix),
        vec![
            (MessageRole::Assistant, "late answer"),
            (MessageRole::User, "follow-up request"),
        ]
    );
    assert!(!suffix.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::AgentMessage(event))
            if event.message == EXTERNAL_SESSION_IMPORTED_MARKER
    )));
}

#[test]
fn interrupted_append_recovers_the_raw_tail_after_the_model_transcript_is_complete() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::Assistant, "late answer"),
    ]);
    let append_base_model_item_count = history_model_items(&history)
        .expect("history model items")
        .len();
    let ExistingSessionAppendPlan::Append(transaction) =
        plan_existing_session_append_from_base(&source, &history, append_base_model_item_count)
            .expect("initial append")
    else {
        panic!("initial transaction must append");
    };
    let response_position = transaction
        .iter()
        .rposition(|item| matches!(item, RolloutItem::ResponseItem(_)))
        .expect("transaction response");
    let mut interrupted_history = history;
    interrupted_history.extend_from_slice(&transaction[..=response_position]);
    assert!(model_transcripts_match(&source, &interrupted_history));

    let ExistingSessionAppendPlan::Append(remainder) = plan_existing_session_append_from_base(
        &source,
        &interrupted_history,
        append_base_model_item_count,
    )
    .expect("recover interrupted append") else {
        panic!("raw tail must still append");
    };

    assert!(!remainder.is_empty());
    assert!(
        remainder
            .iter()
            .all(|item| !matches!(item, RolloutItem::ResponseItem(_)))
    );
    interrupted_history.extend(remainder);
    assert!(matches!(
        plan_existing_session_append_from_base(
            &source,
            &interrupted_history,
            append_base_model_item_count,
        ),
        Some(ExistingSessionAppendPlan::AlreadyComplete)
    ));
}

#[test]
fn interrupted_append_keeps_its_raw_tail_when_the_source_advances() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let second_source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::Assistant, "late answer"),
    ]);
    let append_base_model_item_count = history_model_items(&history)
        .expect("history model items")
        .len();
    let ExistingSessionAppendPlan::Append(transaction) = plan_existing_session_append_from_base(
        &second_source,
        &history,
        append_base_model_item_count,
    )
    .expect("initial append") else {
        panic!("initial transaction must append");
    };
    let response_position = transaction
        .iter()
        .rposition(|item| matches!(item, RolloutItem::ResponseItem(_)))
        .expect("transaction response");
    let mut interrupted_history = history;
    interrupted_history.extend_from_slice(&transaction[..=response_position]);
    let third_source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::Assistant, "late answer"),
        (MessageRole::User, "newer request"),
    ]);

    let ExistingSessionAppendPlan::Append(remainder) = plan_existing_session_append_from_base(
        &third_source,
        &interrupted_history,
        append_base_model_item_count,
    )
    .expect("recover advanced source") else {
        panic!("older raw tail and newer turn must append");
    };

    assert!(matches!(
        remainder.first(),
        Some(RolloutItem::EventMsg(EventMsg::TokenCount(_)))
    ));
    assert_eq!(
        model_messages(&remainder),
        vec![(MessageRole::User, "newer request")]
    );
    interrupted_history.extend(remainder);
    assert!(matches!(
        plan_existing_session_append_from_base(
            &third_source,
            &interrupted_history,
            append_base_model_item_count,
        ),
        Some(ExistingSessionAppendPlan::AlreadyComplete)
    ));
}

#[test]
fn pre_cap_oversized_history_can_receive_a_bounded_suffix() {
    let oversized = "legacy oversized message".repeat(1_000);
    let mut history = rollout(&[(MessageRole::User, &oversized)]);
    for item in &mut history {
        if let RolloutItem::ResponseItem(response) = item
            && let ResponseItem::Message { content, .. } = &mut response.item
            && let [ContentItem::InputText { text }] = content.as_mut_slice()
        {
            *text = oversized.clone();
        }
    }
    let source = rollout(&[
        (MessageRole::User, &oversized),
        (MessageRole::Assistant, "new bounded answer"),
    ]);

    let suffix = plan_append(&source, &history).expect("legacy prefix should remain compatible");

    assert_eq!(
        model_messages(&suffix),
        vec![(MessageRole::Assistant, "new bounded answer")]
    );
}

#[test]
fn large_raw_transaction_prefix_is_recovered_linearly() {
    let raw_item = RolloutItem::SecurityRiskScore(SecurityRiskScore {
        scores: BTreeMap::new(),
        call_id: None,
        action: None,
        sampled_at: None,
    });
    let transaction = vec![raw_item; 8_192];
    let mut history = vec![RolloutItem::EventMsg(EventMsg::AgentMessage(
        codex_protocol::protocol::AgentMessageEvent {
            message: "baseline".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
        },
    ))];
    history.extend_from_slice(&transaction[..6_000]);

    assert_eq!(
        matching_transaction_prefix_len(
            &transaction,
            &history,
            /*expected_model_item_count*/ 0,
        ),
        Some(6_000)
    );
}

#[test]
fn interrupted_append_rejects_a_nontransactional_raw_tail() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::Assistant, "late answer"),
    ]);
    let append_base_model_item_count = history_model_items(&history)
        .expect("history model items")
        .len();
    let ExistingSessionAppendPlan::Append(transaction) =
        plan_existing_session_append_from_base(&source, &history, append_base_model_item_count)
            .expect("initial append")
    else {
        panic!("initial transaction must append");
    };
    let response_position = transaction
        .iter()
        .rposition(|item| matches!(item, RolloutItem::ResponseItem(_)))
        .expect("transaction response");
    let mut divergent_history = history;
    divergent_history.extend_from_slice(&transaction[..=response_position]);
    divergent_history.push(RolloutItem::EventMsg(EventMsg::AgentMessage(
        codex_protocol::protocol::AgentMessageEvent {
            message: "unrelated concurrent append".to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
        },
    )));

    assert!(
        plan_existing_session_append_from_base(
            &source,
            &divergent_history,
            append_base_model_item_count,
        )
        .is_none()
    );
}

#[test]
fn requires_a_strict_nonempty_model_prefix() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let source = rollout(&[
        (MessageRole::User, "first request"),
        (MessageRole::User, "follow-up request"),
    ]);
    assert!(plan_append(&history, &history).is_none());
    assert!(
        plan_append(
            &source,
            &rollout(&[(MessageRole::User, "rewritten request")])
        )
        .is_none()
    );

    let mut metadata_changed = history.clone();
    for item in &mut metadata_changed {
        if let RolloutItem::EventMsg(EventMsg::TurnStarted(event)) = item {
            event.turn_id = "different-turn-id".to_string();
            event.started_at = Some(9_999);
        }
    }
    let security_risk = RolloutItem::SecurityRiskScore(SecurityRiskScore {
        scores: BTreeMap::from([("action_risk".to_string(), 0.92)]),
        call_id: None,
        action: None,
        sampled_at: None,
    });
    metadata_changed.push(security_risk.clone());
    assert!(model_transcripts_match(&history, &metadata_changed));
    assert!(!model_transcripts_match(&source, &history));
    assert!(plan_append(&source, &metadata_changed).is_some());
    let mut source_with_security_risk = source.clone();
    source_with_security_risk.push(security_risk);
    assert!(plan_append(&source_with_security_risk, &metadata_changed).is_none());

    for event in [
        EventMsg::ContextCompacted(ContextCompactedEvent),
        EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 }),
    ] {
        let mut rewritten = history.clone();
        rewritten.push(RolloutItem::EventMsg(event));
        assert!(plan_append(&source, &rewritten).is_none());
    }
    let mut with_tool_call = history;
    with_tool_call.push(RolloutItem::ResponseItem(
        ResponseItem::FunctionCall {
            id: None,
            name: "native_tool".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            call_id: "native-call".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    ));
    assert!(plan_append(&source, &with_tool_call).is_none());
}

#[test]
fn legacy_claim_delivery_appends_from_claim_through_its_later_turn_start() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let mut source = history.clone();
    let mut communication = codex_protocol::protocol::InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "durable result".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(codex_protocol::ResponseItemId::with_suffix(
        "msg",
        "durable-delivery",
    ));
    let delivered_response =
        codex_rollout::ResponseItemEnvelope::new(communication.to_model_input_item());
    source.extend([
        RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id: communication.id.clone().expect("communication id"),
            communication: Some(communication.clone()),
            delivered_response: None,
            fingerprint: "fingerprint".to_string(),
            turn_id: "turn-2".to_string(),
        },
        RolloutItem::DurableInterAgentCommunication {
            communication,
            start_options: codex_protocol::turn_input::TurnStartOptions::default(),
            fingerprint: "fingerprint".to_string(),
            turn_id: "turn-2".to_string(),
            compatibility_shadow: false,
        },
        RolloutItem::ResponseItem(delivered_response),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: "turn-2".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            },
        )),
    ]);

    let suffix = plan_append(&source, &history).expect("durable delivery should append");

    assert!(matches!(
        suffix.first(),
        Some(RolloutItem::DurableInterAgentCommunicationClaimed {
            delivered_response: None,
            ..
        })
    ));
    assert!(matches!(
        suffix.last(),
        Some(RolloutItem::EventMsg(EventMsg::TurnStarted(event))) if event.turn_id == "turn-2"
    ));
    assert!(model_transcripts_match(&source, &source));
}

#[test]
fn legacy_claim_batch_appends_from_the_first_missing_claim() {
    let base_history = rollout(&[(MessageRole::User, "first request")]);
    let mut source = base_history.clone();
    let communications = ["first durable result", "second durable result"].map(|content| {
        let mut communication = codex_protocol::protocol::InterAgentCommunication::new(
            codex_protocol::AgentPath::root(),
            codex_protocol::AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            content.to_string(),
            /*trigger_turn*/ true,
        );
        communication.id = Some(codex_protocol::ResponseItemId::with_suffix("msg", content));
        communication
    });
    for (index, communication) in communications.iter().enumerate() {
        source.extend([
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id: communication.id.clone().expect("communication id"),
                communication: Some(communication.clone()),
                delivered_response: None,
                fingerprint: format!("fingerprint-{index}"),
                turn_id: "turn-2".to_string(),
            },
            RolloutItem::DurableInterAgentCommunication {
                communication: communication.clone(),
                start_options: codex_protocol::turn_input::TurnStartOptions::default(),
                fingerprint: format!("fingerprint-{index}"),
                turn_id: "turn-2".to_string(),
                compatibility_shadow: false,
            },
            RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
                communication.to_model_input_item(),
            )),
        ]);
    }
    source.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
        codex_protocol::protocol::TurnStartedEvent {
            turn_id: "turn-2".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        },
    )));
    let mut partial_history = base_history;
    partial_history.push(RolloutItem::ResponseItem(
        codex_rollout::ResponseItemEnvelope::new(communications[0].to_model_input_item()),
    ));

    let suffix = plan_append(&source, &partial_history).expect("remaining mail should append");

    assert!(matches!(
        suffix.first(),
        Some(RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            ..
        }) if communication_id == communications[1].id.as_ref().expect("communication id")
    ));
    assert!(matches!(
        suffix.last(),
        Some(RolloutItem::EventMsg(EventMsg::TurnStarted(event))) if event.turn_id == "turn-2"
    ));
    partial_history.extend(suffix);
    assert!(model_transcripts_match(&source, &partial_history));
}

#[test]
fn duplicate_claim_identity_keeps_the_first_raw_append_boundary() {
    let history = rollout(&[(MessageRole::User, "first request")]);
    let mut source = history.clone();
    let mut communication = codex_protocol::protocol::InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "durable result".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(codex_protocol::ResponseItemId::with_suffix(
        "msg",
        "duplicate-boundary",
    ));
    let first_claim = RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: communication.id.clone().expect("communication id"),
        communication: Some(communication.clone()),
        delivered_response: None,
        fingerprint: "fingerprint".to_string(),
        turn_id: "turn-2".to_string(),
    };
    source.extend([
        first_claim.clone(),
        RolloutItem::DurableInterAgentCommunication {
            communication: communication.clone(),
            start_options: codex_protocol::turn_input::TurnStartOptions::default(),
            fingerprint: "fingerprint".to_string(),
            turn_id: "turn-2".to_string(),
            compatibility_shadow: false,
        },
        first_claim,
        RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
            communication.to_model_input_item(),
        )),
    ]);

    let suffix = plan_append(&source, &history).expect("durable delivery should append");

    assert!(matches!(
        suffix.first(),
        Some(RolloutItem::DurableInterAgentCommunicationClaimed { .. })
    ));
    assert!(matches!(
        suffix.get(1),
        Some(RolloutItem::DurableInterAgentCommunication { .. })
    ));
}

fn rollout(messages: &[(MessageRole, &str)]) -> Vec<RolloutItem> {
    rollout_items_from_messages(
        messages
            .iter()
            .enumerate()
            .map(|(index, &(role, text))| ConversationMessage {
                role,
                text: text.to_string(),
                timestamp: Some(index as i64),
            })
            .collect(),
    )
}

fn model_messages(items: &[RolloutItem]) -> Vec<(MessageRole, &str)> {
    items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(response_item) => {
                let ResponseItem::Message { role, content, .. } = &response_item.item else {
                    return None;
                };
                match (role.as_str(), content.as_slice()) {
                    ("user", [ContentItem::InputText { text }]) => {
                        Some((MessageRole::User, text.as_str()))
                    }
                    ("assistant", [ContentItem::OutputText { text }]) => {
                        Some((MessageRole::Assistant, text.as_str()))
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}
