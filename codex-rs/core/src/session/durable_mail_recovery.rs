use std::collections::HashMap;
use std::collections::HashSet;

use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;

use super::input_queue::MailboxModelSizeLimit;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingDurableMail {
    pub(super) communication: InterAgentCommunication,
    pub(super) start_options: TurnStartOptions,
    pub(super) fingerprint: String,
    pub(super) intended_turn_id: String,
    pub(super) already_claimed: bool,
    pub(super) size_limit: MailboxModelSizeLimit,
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
    index: usize,
    communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    intended_turn_id: String,
    turn_started: bool,
    size_limit: MailboxModelSizeLimit,
}

struct DurableClaim {
    index: usize,
    turn_id: String,
    communication: Option<InterAgentCommunication>,
    delivered: bool,
}

#[derive(Default)]
struct DurableState {
    payload: Option<DurablePayload>,
    claim: Option<DurableClaim>,
}

struct RecoveryEntry {
    order_index: usize,
    latest_index: usize,
    delivery_index: Option<usize>,
    id: ResponseItemId,
    fingerprint: String,
    payload: Option<DurablePayload>,
    claim: Option<DurableClaim>,
}

impl RecoveryEntry {
    fn delivered(&self) -> bool {
        self.delivery_index.is_some()
    }
}

fn insert_recovery_entry(
    entries_by_id: &mut HashMap<ResponseItemId, RecoveryEntry>,
    entry: RecoveryEntry,
) {
    let replace = match entries_by_id.get(&entry.id) {
        Some(current) => match (current.delivery_index, entry.delivery_index) {
            (Some(current_index), Some(entry_index)) => entry_index < current_index,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => entry.latest_index > current.latest_index,
        },
        None => true,
    };
    if replace {
        entries_by_id.insert(entry.id.clone(), entry);
    }
}

pub(super) fn recover_durable_mail(items: &[RolloutItem]) -> DurableMailRecovery {
    let mut future_turn_starts = HashSet::new();
    let mut claim_delivered = HashMap::new();
    let mut payload_turn_started = HashMap::new();
    for (index, item) in items.iter().enumerate().rev() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                future_turn_starts.insert(event.turn_id.clone());
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                delivered_response,
                turn_id,
                ..
            } => {
                claim_delivered.insert(
                    index,
                    delivered_response.is_some() || future_turn_starts.contains(turn_id),
                );
            }
            RolloutItem::DurableInterAgentCommunication { turn_id, .. } => {
                payload_turn_started.insert(index, future_turn_starts.contains(turn_id));
            }
            _ => {}
        }
    }

    let mut states = HashMap::<(ResponseItemId, String), DurableState>::new();
    let mut legacy_consumed = HashMap::<ResponseItemId, (usize, String)>::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            RolloutItem::DurableInterAgentCommunication {
                communication,
                start_options,
                fingerprint,
                turn_id,
                compatibility_shadow,
            } => {
                let Some(id) = communication.id.clone() else {
                    continue;
                };
                states.entry((id, fingerprint.clone())).or_default().payload =
                    Some(DurablePayload {
                        index,
                        communication: communication.clone(),
                        start_options: start_options.clone(),
                        intended_turn_id: turn_id.clone(),
                        turn_started: payload_turn_started.get(&index).copied().unwrap_or(false),
                        size_limit: if *compatibility_shadow {
                            MailboxModelSizeLimit::Current
                        } else {
                            MailboxModelSizeLimit::LegacyRecovery
                        },
                    });
            }
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id,
                communication,
                fingerprint,
                turn_id,
                ..
            } => {
                let claim = DurableClaim {
                    index,
                    turn_id: turn_id.clone(),
                    communication: communication.clone(),
                    delivered: claim_delivered.get(&index).copied().unwrap_or(false),
                };
                let state = states
                    .entry((communication_id.clone(), fingerprint.clone()))
                    .or_default();
                if state
                    .claim
                    .as_ref()
                    .is_none_or(|current| !current.delivered)
                {
                    state.claim = Some(claim);
                }
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: Some(communication),
                mail_start_options: Some(start_options),
                consumed_mail_id,
                ..
            } => {
                let Some(id) = communication.id.clone() else {
                    continue;
                };
                states
                    .entry((id.clone(), fingerprint.clone()))
                    .or_default()
                    .payload = Some(DurablePayload {
                    index,
                    communication: communication.clone(),
                    start_options: start_options.clone(),
                    intended_turn_id: communication
                        .internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.clone())
                        .unwrap_or_default(),
                    turn_started: false,
                    size_limit: MailboxModelSizeLimit::LegacyRecovery,
                });
                if let Some(consumed_id) = consumed_mail_id {
                    legacy_consumed
                        .entry(consumed_id.clone())
                        .or_insert_with(|| (index, fingerprint.clone()));
                }
            }
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                consumed_mail_id: Some(id),
                ..
            } => {
                legacy_consumed
                    .entry(id.clone())
                    .or_insert_with(|| (index, fingerprint.clone()));
            }
            _ => {}
        }
    }

    // `.mcp.10` persisted the fingerprint immediately before the model-visible response.
    for (index, pair) in items.windows(2).enumerate() {
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
        if let Some(id) = response_item.id().cloned() {
            legacy_consumed
                .entry(id)
                .or_insert_with(|| (index + 1, fingerprint.clone()));
        }
    }

    let mut entries_by_id = HashMap::<ResponseItemId, RecoveryEntry>::new();
    for ((id, fingerprint), state) in states {
        let latest_index = state
            .payload
            .as_ref()
            .map(|payload| payload.index)
            .into_iter()
            .chain(state.claim.as_ref().map(|claim| claim.index))
            .max()
            .unwrap_or_default();
        let matching_legacy_consumed = legacy_consumed
            .get(&id)
            .filter(|(_, consumed_fingerprint)| consumed_fingerprint == &fingerprint);
        let latest_index =
            matching_legacy_consumed.map_or(latest_index, |(index, _)| latest_index.max(*index));
        let order_index = state
            .payload
            .as_ref()
            .map(|payload| payload.index)
            .or_else(|| state.claim.as_ref().map(|claim| claim.index))
            .or_else(|| matching_legacy_consumed.map(|(index, _)| *index))
            .unwrap_or_default();
        let delivery_index = state
            .claim
            .as_ref()
            .filter(|claim| claim.delivered)
            .map(|claim| claim.index)
            .into_iter()
            .chain(matching_legacy_consumed.map(|(index, _)| *index))
            .min();
        let entry = RecoveryEntry {
            order_index,
            latest_index,
            delivery_index,
            id: id.clone(),
            fingerprint,
            payload: state.payload,
            claim: state.claim,
        };
        insert_recovery_entry(&mut entries_by_id, entry);
    }
    for (id, (index, fingerprint)) in legacy_consumed {
        let entry = RecoveryEntry {
            order_index: index,
            latest_index: index,
            delivery_index: Some(index),
            id: id.clone(),
            fingerprint,
            payload: None,
            claim: None,
        };
        insert_recovery_entry(&mut entries_by_id, entry);
    }

    let mut entries = entries_by_id.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.order_index);
    let mut recovery = DurableMailRecovery::default();
    for entry in entries {
        let claimed_turn_id = entry
            .claim
            .as_ref()
            .map(|claim| claim.turn_id.clone())
            .or_else(|| {
                entry
                    .payload
                    .as_ref()
                    .map(|payload| payload.intended_turn_id.clone())
            })
            .unwrap_or_default();
        if entry.delivered() {
            recovery.consumed.push(ConsumedDurableMail {
                id: entry.id,
                fingerprint: entry.fingerprint,
                claimed_turn_id,
            });
            continue;
        }
        let already_claimed = entry.claim.is_some()
            || entry
                .payload
                .as_ref()
                .is_some_and(|payload| payload.turn_started);
        let claim_communication = entry
            .claim
            .as_ref()
            .and_then(|claim| claim.communication.clone());
        let (communication, start_options, size_limit) =
            if let Some(communication) = claim_communication {
                let start_options = entry
                    .payload
                    .as_ref()
                    .map(|payload| payload.start_options.clone())
                    .unwrap_or_default();
                (
                    communication,
                    start_options,
                    MailboxModelSizeLimit::LegacyRecovery,
                )
            } else if let Some(payload) = entry.payload {
                (
                    payload.communication,
                    payload.start_options,
                    payload.size_limit,
                )
            } else {
                // A claim without any recoverable body is consumed fail-closed: replaying an unknown
                // MCP-era mailbox occurrence would risk duplicating a side effect.
                recovery.consumed.push(ConsumedDurableMail {
                    id: entry.id,
                    fingerprint: entry.fingerprint,
                    claimed_turn_id,
                });
                continue;
            };
        recovery.pending.push(PendingDurableMail {
            communication,
            start_options,
            fingerprint: entry.fingerprint,
            intended_turn_id: claimed_turn_id,
            already_claimed,
            size_limit,
        });
    }
    recovery
}

#[cfg(test)]
#[path = "durable_mail_recovery_tests.rs"]
mod tests;
