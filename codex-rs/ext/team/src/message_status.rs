//! Merge host-owned evidence with synchronized coordinator observations.

use crate::TeamHostService;
use anyhow::Result;
use codex_app_server_protocol::*;
use std::collections::BTreeMap;
use std::collections::HashSet;

impl TeamHostService {
    pub(crate) async fn agent_message_status(
        &self,
        sender: &TeamAddress,
        query: TeamMessageStatusParams,
    ) -> Result<TeamMessageStatusResponse> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let directory = store
            .get::<TeamDirectorySnapshot>("directory_cache", "current")
            .await?
            .unwrap_or_default();
        let name = |address: &TeamAddress| {
            directory
                .data
                .iter()
                .find(|entry| &entry.address == address)
                .map(|entry| entry.alias.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}/{}/{}",
                        address.machine_id, address.server_id, address.thread_id
                    )
                })
        };
        let mut statuses = BTreeMap::new();

        // ACK means intake was recorded, including rejection. Keep local queued
        // evidence until an explicit synchronized outcome supersedes it.
        for entry in store.message_outbox_evidence().await? {
            let message = entry
                .payload
                .get("message")
                .and_then(|value| serde_json::from_value::<TeamMessage>(value.clone()).ok());
            let author = message
                .as_ref()
                .map(|message| message.sender.clone())
                .or_else(|| {
                    let action = entry
                        .payload
                        .pointer("/request/type")
                        .and_then(serde_json::Value::as_str);
                    matches!(action, Some("post" | "reply"))
                        .then(|| {
                            entry
                                .payload
                                .get("sender")
                                .and_then(|value| serde_json::from_value(value.clone()).ok())
                        })
                        .flatten()
                });
            if author.as_ref() != Some(sender) {
                continue;
            }
            let message_id = message
                .as_ref()
                .map_or(entry.id, |message| message.id.clone());
            statuses.insert(
                message_id.clone(),
                TeamMessageStatus {
                    message_id,
                    sender: sender.clone(),
                    sender_name: name(sender),
                    created_at: message.as_ref().map(|message| message.created_at),
                    intake: TeamMessageIntake::QueuedOnSenderHost,
                    recipients: message
                        .as_ref()
                        .and_then(|message| message.recipient.as_ref())
                        .map(|address| TeamRecipientMessageStatus {
                            address: address.clone(),
                            name: name(address),
                            progress: TeamMessageProgress::QueuedOnSenderHost,
                            receipt: None,
                            abandonment: None,
                            freshness: TeamObservationFreshness::Current,
                        })
                        .into_iter()
                        .collect(),
                    related_replies: Vec::new(),
                    detail: None,
                    freshness: TeamObservationFreshness::Current,
                },
            );
        }

        for mut status in store
            .get::<TeamMessageStatusSnapshot>("message_status_cache", "current")
            .await?
            .unwrap_or_default()
            .data
        {
            let sent = status.sender == *sender;
            if !sent
                && !status
                    .recipients
                    .iter()
                    .any(|recipient| recipient.address == *sender)
            {
                continue;
            }
            // Recipient credentials expose their own evidence only. A sender
            // can inspect the frozen audience of messages they authored.
            if !sent {
                status
                    .recipients
                    .retain(|recipient| recipient.address == *sender);
            }
            status.freshness = TeamObservationFreshness::LastSynchronized;
            for recipient in &mut status.recipients {
                if recipient.freshness == TeamObservationFreshness::Current {
                    recipient.freshness = TeamObservationFreshness::LastSynchronized;
                }
            }
            statuses.insert(status.message_id.clone(), status);
        }

        for (delivery, receipt) in store.deliveries(&sender.thread_id).await? {
            let status = statuses
                .entry(delivery.message.id.clone())
                .or_insert_with(|| TeamMessageStatus {
                    message_id: delivery.message.id.clone(),
                    sender: delivery.message.sender.clone(),
                    sender_name: name(&delivery.message.sender),
                    created_at: Some(delivery.message.created_at),
                    intake: TeamMessageIntake::AcceptedAtCoordinator,
                    recipients: Vec::new(),
                    related_replies: Vec::new(),
                    detail: None,
                    freshness: TeamObservationFreshness::Current,
                });
            status
                .recipients
                .retain(|recipient| recipient.address != *sender);
            status.recipients.push(TeamRecipientMessageStatus {
                address: sender.clone(),
                name: name(sender),
                progress: receipt.state.into(),
                receipt: Some(receipt),
                abandonment: store.get("delivery_abandonment", &delivery.id).await?,
                freshness: TeamObservationFreshness::Current,
            });
        }
        let visible = statuses.keys().cloned().collect::<HashSet<_>>();
        let mut data = statuses
            .into_values()
            .filter(|status| {
                query
                    .message_id
                    .as_ref()
                    .is_none_or(|id| id == &status.message_id)
            })
            .filter(|status| {
                query
                    .cursor
                    .as_ref()
                    .is_none_or(|cursor| &status.message_id > cursor)
            })
            .map(|mut status| {
                status.related_replies.retain(|id| visible.contains(id));
                status
            })
            .collect::<Vec<_>>();
        let limit = query.limit.unwrap_or(100).clamp(1, 1000) as usize;
        let next_cursor = if data.len() > limit {
            data.truncate(limit);
            data.last().map(|status| status.message_id.clone())
        } else {
            None
        };
        Ok(TeamMessageStatusResponse { data, next_cursor })
    }
}
