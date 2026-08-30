use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::turn_input::TurnStartOptions;
use pretty_assertions::assert_eq;

use super::ConsumedDurableMail;
use super::DurableMailRecovery;
use super::PendingDurableMail;
use super::recover_durable_mail;

fn mail(id: &str) -> InterAgentCommunication {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "durable payload".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(ResponseItemId::with_suffix("msg", id));
    communication
}

fn payload(communication: &InterAgentCommunication) -> RolloutItem {
    RolloutItem::DurableInterAgentCommunication {
        communication: communication.clone(),
        start_options: TurnStartOptions::default(),
        fingerprint: "fingerprint".to_string(),
        turn_id: "intended-turn".to_string(),
    }
}

fn claim(communication: &InterAgentCommunication) -> RolloutItem {
    RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: communication.id.clone().expect("message id"),
        communication: Some(communication.clone()),
        fingerprint: "fingerprint".to_string(),
        turn_id: "claimed-turn".to_string(),
    }
}

#[test]
fn current_unclaimed_payload_is_recovered_as_pending() {
    let communication = mail("pending");

    assert_eq!(
        recover_durable_mail(&[payload(&communication)]),
        DurableMailRecovery {
            pending: vec![PendingDurableMail {
                communication,
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                intended_turn_id: "intended-turn".to_string(),
                already_claimed: false,
            }],
            consumed: Vec::new(),
        }
    );
}

#[test]
fn claim_without_turn_start_is_recovered_for_the_same_turn() {
    let communication = mail("claim-tail");

    assert_eq!(
        recover_durable_mail(&[payload(&communication), claim(&communication)]),
        DurableMailRecovery {
            pending: vec![PendingDurableMail {
                communication,
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                intended_turn_id: "claimed-turn".to_string(),
                already_claimed: true,
            }],
            consumed: Vec::new(),
        }
    );
}

#[test]
fn claim_with_turn_start_is_recovered_as_consumed() {
    let communication = mail("started");
    let id = communication.id.clone().expect("message id");

    assert_eq!(
        recover_durable_mail(&[
            payload(&communication),
            claim(&communication),
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "claimed-turn".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
        ]),
        DurableMailRecovery {
            pending: Vec::new(),
            consumed: vec![ConsumedDurableMail {
                id,
                fingerprint: "fingerprint".to_string(),
                claimed_turn_id: "claimed-turn".to_string(),
            }],
        }
    );
}

#[test]
fn mcp11_overloaded_metadata_retains_pending_mail() {
    let communication = mail("mcp11");

    assert_eq!(
        recover_durable_mail(&[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: true,
            mail_fingerprint: Some("legacy-fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: Some(communication.clone()),
            mail_start_options: Some(TurnStartOptions::default()),
            consumed_mail_id: None,
        }]),
        DurableMailRecovery {
            pending: vec![PendingDurableMail {
                communication,
                start_options: TurnStartOptions::default(),
                fingerprint: "legacy-fingerprint".to_string(),
                intended_turn_id: String::new(),
                already_claimed: false,
            }],
            consumed: Vec::new(),
        }
    );
}

#[test]
fn mcp10_fingerprint_pair_is_restored_as_consumed() {
    let communication = mail("mcp10");
    let id = communication.id.clone().expect("message id");

    assert_eq!(
        recover_durable_mail(&[
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: true,
                mail_fingerprint: Some("legacy-fingerprint".to_string()),
                delivery_id: None,
                terminal_turn_id: None,
                terminal_status: None,
                mail_communication: None,
                mail_start_options: None,
                consumed_mail_id: None,
            },
            RolloutItem::ResponseItem(communication.to_model_input_item().into()),
        ]),
        DurableMailRecovery {
            pending: Vec::new(),
            consumed: vec![ConsumedDurableMail {
                id,
                fingerprint: "legacy-fingerprint".to_string(),
                claimed_turn_id: String::new(),
            }],
        }
    );
}

#[test]
fn compacted_consumed_receipt_restores_retry_identity_without_model_mail() {
    let id = ResponseItemId::with_suffix("msg", "compacted");

    assert_eq!(
        recover_durable_mail(&[RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: Some("fingerprint".to_string()),
            delivery_id: None,
            terminal_turn_id: None,
            terminal_status: None,
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: Some(id.clone()),
        }]),
        DurableMailRecovery {
            pending: Vec::new(),
            consumed: vec![ConsumedDurableMail {
                id,
                fingerprint: "fingerprint".to_string(),
                claimed_turn_id: String::new(),
            }],
        }
    );
}
