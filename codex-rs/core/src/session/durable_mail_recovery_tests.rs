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
use crate::session::input_queue::MailboxModelSizeLimit;

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
        compatibility_shadow: true,
    }
}

fn claim(communication: &InterAgentCommunication) -> RolloutItem {
    RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: communication.id.clone().expect("message id"),
        communication: None,
        delivered_response: None,
        fingerprint: "fingerprint".to_string(),
        turn_id: "claimed-turn".to_string(),
    }
}

fn mcp13_claim(communication: &InterAgentCommunication) -> RolloutItem {
    RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: communication.id.clone().expect("message id"),
        communication: Some(communication.clone()),
        delivered_response: None,
        fingerprint: "fingerprint".to_string(),
        turn_id: "claimed-turn".to_string(),
    }
}

fn committed_delivery(communication: &InterAgentCommunication) -> RolloutItem {
    RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: communication.id.clone().expect("message id"),
        communication: Some(communication.clone()),
        delivered_response: Some(communication.to_model_input_item().into()),
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
                size_limit: MailboxModelSizeLimit::Current,
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
                size_limit: MailboxModelSizeLimit::Current,
            }],
            consumed: Vec::new(),
        }
    );
}

#[test]
fn pending_only_payload_with_turn_start_but_without_delivery_is_recovered_as_pending() {
    let communication = mail("started");

    assert_eq!(
        recover_durable_mail(&[
            RolloutItem::DurableInterAgentCommunication {
                communication: communication.clone(),
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                turn_id: "intended-turn".to_string(),
                compatibility_shadow: true,
            },
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "unrelated-turn".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
        ]),
        DurableMailRecovery {
            pending: vec![PendingDurableMail {
                communication,
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                intended_turn_id: "intended-turn".to_string(),
                already_claimed: false,
                size_limit: MailboxModelSizeLimit::Current,
            }],
            consumed: Vec::new(),
        }
    );
}

#[test]
fn current_delivery_is_consumed_only_after_its_exact_model_item_is_persisted() {
    let communication = mail("delivered");
    let id = communication.id.clone().expect("message id");

    assert_eq!(
        recover_durable_mail(&[payload(&communication), committed_delivery(&communication),]),
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
fn mcp13_claim_with_turn_start_retains_legacy_consumption_semantics() {
    let communication = mail("mcp13");
    let id = communication.id.clone().expect("message id");

    assert_eq!(
        recover_durable_mail(&[
            payload(&communication),
            mcp13_claim(&communication),
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
fn earlier_turn_start_does_not_consume_a_later_mcp13_claim() {
    let communication = mail("mcp13-prior-turn");

    assert_eq!(
        recover_durable_mail(&[
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "claimed-turn".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
            mcp13_claim(&communication),
        ]),
        DurableMailRecovery {
            pending: vec![PendingDurableMail {
                communication,
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                intended_turn_id: "claimed-turn".to_string(),
                already_claimed: true,
                size_limit: MailboxModelSizeLimit::LegacyRecovery,
            }],
            consumed: Vec::new(),
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
                size_limit: MailboxModelSizeLimit::LegacyRecovery,
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

#[test]
fn claim_only_recovery_preserves_fifo_order() {
    let first = mail("claim-first");
    let second = mail("claim-second");

    let recovered = recover_durable_mail(&[mcp13_claim(&first), mcp13_claim(&second)]);

    assert_eq!(
        recovered
            .pending
            .iter()
            .map(|pending| pending.communication.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
}

#[test]
fn mixed_payload_and_claim_only_recovery_preserves_global_fifo_order() {
    let first = mail("payload-first");
    let second = mail("claim-second");
    let third = mail("payload-third");

    let recovered = recover_durable_mail(&[payload(&first), mcp13_claim(&second), payload(&third)]);

    assert_eq!(
        recovered
            .pending
            .iter()
            .map(|pending| pending.communication.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id, second.id, third.id]
    );
}

#[test]
fn a_later_unmatched_claim_does_not_move_mail_behind_newer_payloads() {
    let first = mail("claimed-first");
    let second = mail("payload-second");

    let recovered = recover_durable_mail(&[payload(&first), payload(&second), claim(&first)]);

    assert_eq!(
        recovered
            .pending
            .iter()
            .map(|pending| pending.communication.id.clone())
            .collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
}

#[test]
fn matching_turn_start_makes_an_unclaimed_payload_require_a_fresh_turn_id() {
    let communication = mail("started-payload");

    let recovered = recover_durable_mail(&[
        payload(&communication),
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "intended-turn".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
    ]);

    assert_eq!(recovered.pending.len(), 1);
    assert!(recovered.pending[0].already_claimed);
}

#[test]
fn latest_exact_payload_wins_for_a_reused_id() {
    let first = mail("latest");
    let mut latest = first.clone();
    latest.content = "latest exact payload".to_string();
    let mut latest_payload = payload(&latest);
    let RolloutItem::DurableInterAgentCommunication { fingerprint, .. } = &mut latest_payload
    else {
        unreachable!("payload helper returns durable mail");
    };
    *fingerprint = "latest-fingerprint".to_string();

    let recovered = recover_durable_mail(&[payload(&first), latest_payload]);

    assert_eq!(
        recovered.pending,
        vec![PendingDurableMail {
            communication: latest,
            start_options: TurnStartOptions::default(),
            fingerprint: "latest-fingerprint".to_string(),
            intended_turn_id: "intended-turn".to_string(),
            already_claimed: false,
            size_limit: MailboxModelSizeLimit::Current,
        }]
    );
}

#[test]
fn later_duplicate_payload_keeps_its_first_admission_fifo_position() {
    let first = mail("duplicate-first");
    let second = mail("second");
    let mut latest = first.clone();
    latest.content = "latest first payload".to_string();
    let mut latest_payload = payload(&latest);
    let RolloutItem::DurableInterAgentCommunication { fingerprint, .. } = &mut latest_payload
    else {
        unreachable!("payload helper returns durable mail");
    };
    *fingerprint = "latest-fingerprint".to_string();

    let recovered = recover_durable_mail(&[payload(&first), payload(&second), latest_payload]);

    assert_eq!(
        recovered
            .pending
            .iter()
            .map(|pending| (&pending.communication, pending.fingerprint.as_str()))
            .collect::<Vec<_>>(),
        vec![(&latest, "latest-fingerprint"), (&second, "fingerprint"),]
    );
}

#[test]
fn latest_cross_fingerprint_claim_keeps_the_original_fifo_position() {
    let first = mail("cross-fingerprint-first");
    let second = mail("cross-fingerprint-second");
    let mut claimed = first.clone();
    claimed.content = "latest claimed body".to_string();
    let latest_claim = RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: first.id.clone().expect("message id"),
        communication: Some(claimed.clone()),
        delivered_response: None,
        fingerprint: "latest-fingerprint".to_string(),
        turn_id: "latest-turn".to_string(),
    };

    let recovered = recover_durable_mail(&[payload(&first), payload(&second), latest_claim]);

    assert_eq!(
        recovered
            .pending
            .iter()
            .map(|pending| (&pending.communication, pending.fingerprint.as_str()))
            .collect::<Vec<_>>(),
        vec![(&claimed, "latest-fingerprint"), (&second, "fingerprint"),]
    );
}

#[test]
fn bodyless_cross_fingerprint_claim_consumes_the_id_fail_closed() {
    let communication = mail("bodyless-cross-fingerprint");
    let id = communication.id.clone().expect("message id");
    let bodyless_claim = RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: id.clone(),
        communication: None,
        delivered_response: None,
        fingerprint: "unknown-fingerprint".to_string(),
        turn_id: "unknown-turn".to_string(),
    };

    assert_eq!(
        recover_durable_mail(&[payload(&communication), bodyless_claim]),
        DurableMailRecovery {
            pending: Vec::new(),
            consumed: vec![ConsumedDurableMail {
                id,
                fingerprint: "unknown-fingerprint".to_string(),
                claimed_turn_id: "unknown-turn".to_string(),
            }],
        }
    );
}

#[test]
fn mcp10_delivery_wins_over_a_later_conflicting_pending_payload() {
    let delivered = mail("mcp10-conflict");
    let id = delivered.id.clone().expect("message id");
    let mut conflicting = delivered.clone();
    conflicting.content = "later conflicting payload".to_string();
    let mut conflicting_payload = payload(&conflicting);
    let RolloutItem::DurableInterAgentCommunication { fingerprint, .. } = &mut conflicting_payload
    else {
        unreachable!("payload helper returns durable mail");
    };
    *fingerprint = "conflicting-fingerprint".to_string();

    assert_eq!(
        recover_durable_mail(&[
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
            RolloutItem::ResponseItem(delivered.to_model_input_item().into()),
            conflicting_payload,
        ]),
        DurableMailRecovery {
            pending: Vec::new(),
            consumed: vec![ConsumedDurableMail {
                id,
                fingerprint: "delivered-fingerprint".to_string(),
                claimed_turn_id: String::new(),
            }],
        }
    );
}

#[test]
fn an_older_delivered_occurrence_wins_over_a_conflicting_pending_reuse() {
    let delivered = mail("delivered-conflict");
    let id = delivered.id.clone().expect("message id");
    let mut conflicting = delivered.clone();
    conflicting.content = "conflicting later payload".to_string();
    let mut conflicting_payload = payload(&conflicting);
    let RolloutItem::DurableInterAgentCommunication { fingerprint, .. } = &mut conflicting_payload
    else {
        unreachable!("payload helper returns durable mail");
    };
    *fingerprint = "conflicting-fingerprint".to_string();

    let recovered = recover_durable_mail(&[committed_delivery(&delivered), conflicting_payload]);

    assert_eq!(
        recovered,
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
fn an_older_delivered_occurrence_wins_over_an_exact_pending_reuse() {
    let delivered = mail("delivered-exact-conflict");
    let id = delivered.id.clone().expect("message id");

    let recovered = recover_durable_mail(&[
        committed_delivery(&delivered),
        payload(&delivered),
        claim(&delivered),
    ]);

    assert_eq!(
        recovered,
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
fn the_first_delivered_fingerprint_wins_for_a_reused_id() {
    let first = mail("delivered-reuse");
    let id = first.id.clone().expect("message id");
    let mut second = first.clone();
    second.content = "later conflicting delivery".to_string();
    let later_delivery = RolloutItem::DurableInterAgentCommunicationClaimed {
        communication_id: id.clone(),
        communication: Some(second.clone()),
        delivered_response: Some(second.to_model_input_item().into()),
        fingerprint: "later-fingerprint".to_string(),
        turn_id: "later-turn".to_string(),
    };

    assert_eq!(
        recover_durable_mail(&[committed_delivery(&first), later_delivery]),
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
fn mcp13_claim_body_supersedes_its_unbounded_compatibility_payload() {
    let bounded = mail("bounded-claim");
    let mut raw = bounded.clone();
    raw.content = "raw legacy body".repeat(20_000);
    let raw_payload = RolloutItem::DurableInterAgentCommunication {
        communication: raw,
        start_options: TurnStartOptions::default(),
        fingerprint: "fingerprint".to_string(),
        turn_id: "claimed-turn".to_string(),
        compatibility_shadow: false,
    };

    let recovered = recover_durable_mail(&[mcp13_claim(&bounded), raw_payload]);

    assert_eq!(recovered.pending[0].communication, bounded);
    assert_eq!(
        recovered.pending[0].size_limit,
        MailboxModelSizeLimit::LegacyRecovery
    );
}
