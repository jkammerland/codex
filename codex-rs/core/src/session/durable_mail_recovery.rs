use std::collections::HashMap;
use std::collections::HashSet;

use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingDurableMail {
    pub(super) communication: InterAgentCommunication,
    pub(super) start_options: TurnStartOptions,
    pub(super) fingerprint: String,
    pub(super) intended_turn_id: String,
    pub(super) already_claimed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ConsumedDurableMail {
    pub(super) id: ResponseItemId,
    pub(super) fingerprint: String,
    pub(super) claimed_turn_id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct DurableMailRecovery {
    pub(super) pending: Vec<PendingDurableMail>,
    pub(super) consumed: Vec<ConsumedDurableMail>,
}

struct DurablePayload {
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    fingerprint: String,
    intended_turn_id: String,
}

struct DurableClaim {
    turn_id: String,
    communication: Option<InterAgentCommunication>,
    delivered: bool,
}

pub(super) fn recover_durable_mail(items: &[RolloutItem]) -> DurableMailRecovery {
    let started_turn_ids = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(event.turn_id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let legacy_consumed_ids = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::InterAgentCommunicationMetadata {
                consumed_mail_id: Some(id),
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let claims = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id,
                communication,
                delivered_response,
                fingerprint,
                turn_id,
            } => Some((
                (communication_id.clone(), fingerprint.clone()),
                DurableClaim {
                    turn_id: turn_id.clone(),
                    communication: communication.clone(),
                    delivered: delivered_response.is_some(),
                },
            )),
            _ => None,
        })
        .collect::<HashMap<_, _>>();

    let mut payloads = Vec::new();
    for item in items {
        match item {
            RolloutItem::DurableInterAgentCommunication {
                communication,
                start_options,
                fingerprint,
                turn_id,
                ..
            } => payloads.push(DurablePayload {
                communication: communication.clone(),
                start_options: start_options.clone(),
                fingerprint: fingerprint.clone(),
                intended_turn_id: turn_id.clone(),
            }),
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: Some(communication),
                mail_start_options: Some(start_options),
                ..
            } => payloads.push(DurablePayload {
                communication: communication.clone(),
                start_options: start_options.clone(),
                fingerprint: fingerprint.clone(),
                intended_turn_id: communication
                    .internal_chat_message_metadata_passthrough
                    .as_ref()
                    .and_then(|metadata| metadata.turn_id.clone())
                    .unwrap_or_default(),
            }),
            _ => {}
        }
    }

    let mut recovery = DurableMailRecovery::default();
    let mut recovered_ids = HashSet::new();
    for payload in payloads {
        let Some(id) = payload.communication.id.clone() else {
            continue;
        };
        if !recovered_ids.insert(id.clone()) {
            continue;
        }
        let claim = claims.get(&(id.clone(), payload.fingerprint.clone()));
        let delivered =
            claim.is_some_and(|claim| claim.delivered || started_turn_ids.contains(&claim.turn_id));
        if legacy_consumed_ids.contains(&id) || delivered {
            recovery.consumed.push(ConsumedDurableMail {
                id,
                fingerprint: payload.fingerprint,
                claimed_turn_id: claim
                    .map(|claim| claim.turn_id.clone())
                    .unwrap_or(payload.intended_turn_id),
            });
        } else {
            recovery.pending.push(PendingDurableMail {
                communication: payload.communication,
                start_options: payload.start_options,
                fingerprint: payload.fingerprint,
                intended_turn_id: claim
                    .map(|claim| claim.turn_id.clone())
                    .unwrap_or(payload.intended_turn_id),
                already_claimed: claim.is_some(),
            });
        }
    }

    // `.mcp.10` persisted the fingerprint directly before an already model-visible response
    // item. Restoring it as consumed preserves idempotent same-ID retries without replaying mail.
    for pair in items.windows(2) {
        let [
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: None,
                ..
            },
            RolloutItem::ResponseItem(response_item),
        ] = pair
        else {
            continue;
        };
        let Some(id) = response_item.id().cloned() else {
            continue;
        };
        if recovered_ids.insert(id.clone()) {
            recovery.consumed.push(ConsumedDurableMail {
                id,
                fingerprint: fingerprint.clone(),
                claimed_turn_id: String::new(),
            });
        }
    }

    // Bounded paginated-history scans collapse already-consumed legacy mail into one
    // model-invisible receipt so an old model-visible response does not leak across the newest
    // compaction checkpoint.
    for item in items {
        let RolloutItem::InterAgentCommunicationMetadata {
            mail_fingerprint: Some(fingerprint),
            consumed_mail_id: Some(id),
            ..
        } = item
        else {
            continue;
        };
        if recovered_ids.insert(id.clone()) {
            recovery.consumed.push(ConsumedDurableMail {
                id: id.clone(),
                fingerprint: fingerprint.clone(),
                claimed_turn_id: String::new(),
            });
        }
    }

    // A `.mcp.13` claim carries enough payload to recover even when the older payload was
    // compacted. Claims from earlier releases omitted that field; without a payload they retain
    // the legacy consumed fallback because replaying an unknown body would be unsafe.
    for ((id, fingerprint), claim) in claims {
        if recovered_ids.insert(id.clone()) {
            if let Some(communication) = claim.communication
                && !claim.delivered
                && !started_turn_ids.contains(&claim.turn_id)
            {
                recovery.pending.push(PendingDurableMail {
                    communication,
                    start_options: TurnStartOptions::default(),
                    fingerprint,
                    intended_turn_id: claim.turn_id,
                    already_claimed: true,
                });
            } else {
                recovery.consumed.push(ConsumedDurableMail {
                    id,
                    fingerprint,
                    claimed_turn_id: claim.turn_id,
                });
            }
        }
    }
    recovery
}

#[cfg(test)]
#[path = "durable_mail_recovery_tests.rs"]
mod tests;
