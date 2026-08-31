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
fn legacy_claim_batch_appends_from_the_earliest_claim() {
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
        }) if communication_id == communications[0].id.as_ref().expect("communication id")
    ));
    assert!(matches!(
        suffix.last(),
        Some(RolloutItem::EventMsg(EventMsg::TurnStarted(event))) if event.turn_id == "turn-2"
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
