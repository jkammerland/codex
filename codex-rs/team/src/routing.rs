use std::collections::BTreeMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::*;

#[derive(Serialize, Deserialize)]
pub(crate) struct Subscription {
    pub sender: TeamAddress,
    pub topic: String,
    pub enabled: bool,
}

#[derive(Deserialize)]
pub(crate) struct OutboundMessage {
    pub message: TeamMessage,
    pub delivery: TeamDeliveryMode,
}

impl Coordinator {
    pub(crate) async fn operator_message(
        &self,
        recipient: Option<TeamAddress>,
        body: String,
        kind: TeamMessageKind,
        reply_to: Option<String>,
        topic: Option<String>,
    ) -> Result<TeamMessage> {
        Ok(TeamMessage {
            id: Uuid::now_v7().to_string(),
            sender: TeamAddress {
                machine_id: "operator".into(),
                server_id: self.current_control().await?.coordinator_id,
                thread_id: "operator".into(),
            },
            recipient,
            kind,
            body,
            reply_to,
            topic,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_secs()
                .try_into()?,
        })
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn ingest(
        &self,
        message: TeamMessage,
        delivery: TeamDeliveryMode,
    ) -> Result<MessageRecord> {
        ensure!(!message.body.trim().is_empty(), "message body is required");
        ensure!(
            Uuid::parse_str(&message.id).is_ok(),
            "message id must be a UUID"
        );
        if message.kind == TeamMessageKind::Board {
            ensure!(
                message.recipient.is_none()
                    && message
                        .topic
                        .as_ref()
                        .is_some_and(|topic| !topic.trim().is_empty()),
                "board posts require a topic and no recipient"
            );
        } else {
            let recipient = message
                .recipient
                .as_ref()
                .context("message recipient is required")?;
            if recipient.machine_id == "operator" {
                ensure!(
                    recipient.server_id == self.current_control().await?.coordinator_id
                        && recipient.thread_id == "operator",
                    "unknown operator inbox"
                );
            } else if recipient.machine_id == "gitea" {
                let original_id = message
                    .reply_to
                    .as_deref()
                    .context("Gitea replies require an original message")?;
                let original = self
                    .store
                    .get::<MessageRecord>("message", original_id)
                    .await?
                    .context("original message is missing")?;
                ensure!(
                    message.kind == TeamMessageKind::Reply
                        && original.message.sender == *recipient
                        && (original.message.recipient.as_ref() == Some(&message.sender)
                            || message.sender.machine_id == "operator")
                        && self
                            .store
                            .get::<crate::gitea::Origin>("gitea_origin", original_id)
                            .await?
                            .is_some(),
                    "reply does not belong to this Gitea conversation"
                );
            } else {
                ensure!(
                    self.store
                        .get::<Agent>("agent", &address_key(recipient))
                        .await?
                        .is_some(),
                    "recipient is not enrolled"
                );
            }
        }
        let _mutation = self.mutations.lock().await;
        if let Some(existing) = self
            .store
            .get::<MessageRecord>("message", &message.id)
            .await?
        {
            ensure!(
                existing.message == message && existing.delivery == delivery,
                "message id was reused with different content"
            );
            return Ok(existing);
        }
        let record = MessageRecord {
            message,
            delivery,
            targets: None,
            receipts: BTreeMap::new(),
            published: false,
            error: None,
            external_url: None,
        };
        self.store
            .put("message", &record.message.id, &record)
            .await?;
        self.notify();
        Ok(record)
    }

    pub(crate) async fn subscribe(
        &self,
        address: TeamAddress,
        topic: String,
        enabled: bool,
    ) -> Result<()> {
        ensure!(!topic.trim().is_empty(), "board topic is required");
        ensure!(
            self.store
                .get::<Agent>("agent", &address_key(&address))
                .await?
                .is_some(),
            "subscriber is not enrolled"
        );
        let key = serde_json::to_string(&(&topic, &address))?;
        self.store
            .put(
                "subscription",
                &key,
                &Subscription {
                    sender: address,
                    topic,
                    enabled,
                },
            )
            .await?;
        self.notify();
        Ok(())
    }

    /// Freeze the audience once, before publishing. Reconnects cannot add new
    /// recipients to an old post or wake an already acknowledged delivery twice.
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn publish(&self) -> Result<()> {
        let _mutation = self.mutations.lock().await;
        if self.current_control().await?.mode != TeamExecutionMode::Running {
            return Ok(());
        }
        let subscriptions = self
            .store
            .list::<Subscription>("subscription", "", u32::MAX)
            .await?;
        for (id, mut record) in self
            .store
            .list::<MessageRecord>("message", "", u32::MAX)
            .await?
        {
            if record.published || record.error.is_some() {
                continue;
            }
            record.targets = Some(match &record.message.recipient {
                Some(recipient)
                    if matches!(recipient.machine_id.as_str(), "gitea" | "operator") =>
                {
                    Vec::new()
                }
                Some(recipient) => vec![recipient.clone()],
                None => subscriptions
                    .iter()
                    .filter(|(_, subscription)| {
                        subscription.enabled
                            && Some(&subscription.topic) == record.message.topic.as_ref()
                            && subscription.sender != record.message.sender
                    })
                    .map(|(_, subscription)| subscription.sender.clone())
                    .collect(),
            });
            record.published = true;
            self.store.put("message", &id, &record).await?;
            self.notify();
        }
        Ok(())
    }

    pub(crate) async fn deliver_to(
        &self,
        machine_id: &str,
        server_id: &str,
        peer: &Peer,
    ) -> Result<()> {
        for (id, record) in self
            .store
            .list::<MessageRecord>("message", "", u32::MAX)
            .await?
        {
            if !record.published || record.error.is_some() {
                continue;
            }
            for address in record.targets.as_deref().unwrap_or_default() {
                if address.machine_id != machine_id || address.server_id != server_id {
                    continue;
                }
                let delivery_id = format!("{id}:{}", address_key(address));
                let agent = self
                    .store
                    .get::<Agent>("agent", &address_key(address))
                    .await?;
                if agent.as_ref().is_some_and(|agent| {
                    agent
                        .runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.archived)
                }) {
                    continue;
                }
                if record
                    .receipts
                    .get(&delivery_id)
                    .is_some_and(|receipt| receipt.state != TeamReceiptState::Pending)
                {
                    continue;
                }

                // The worker applies the same durable control before dispatch.
                // A concurrent pause may leave an admitted request in flight;
                // its per-machine acknowledgement remains visible to the user.
                if self.current_control().await?.mode != TeamExecutionMode::Running {
                    return Ok(());
                }
                if agent.is_some_and(|agent| agent.runtime.is_some_and(|runtime| !runtime.loaded)) {
                    let _: ThreadResumeResponse = peer
                        .handle
                        .request_typed(ClientRequest::ThreadResume {
                            request_id: request_id(),
                            params: ThreadResumeParams {
                                thread_id: address.thread_id.clone(),
                                ..Default::default()
                            },
                        })
                        .await?;
                }
                if record.receipts.contains_key(&delivery_id) {
                    continue;
                }
                let response: TeamDeliverResponse = peer
                    .handle
                    .request_typed(ClientRequest::TeamDeliver {
                        request_id: request_id(),
                        params: TeamDeliverParams {
                            coordinator_id: self.current_control().await?.coordinator_id,
                            delivery: TeamDelivery {
                                id: delivery_id,
                                thread_id: address.thread_id.clone(),
                                message: record.message.clone(),
                                mode: record.delivery,
                                wake: record.message.kind != TeamMessageKind::Note,
                            },
                        },
                    })
                    .await?;
                self.receipt(machine_id, server_id, response.receipt)
                    .await?;
            }
        }
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn receipt(
        &self,
        machine_id: &str,
        server_id: &str,
        receipt: TeamReceipt,
    ) -> Result<()> {
        let message_id = receipt
            .delivery_id
            .split_once(':')
            .context("invalid delivery id")?
            .0;
        let _mutation = self.mutations.lock().await;
        let mut record = self
            .store
            .get::<MessageRecord>("message", message_id)
            .await?
            .context("receipt has no message")?;
        let address = TeamAddress {
            machine_id: machine_id.into(),
            server_id: server_id.into(),
            thread_id: receipt.thread_id.clone(),
        };
        ensure!(
            record
                .targets
                .as_ref()
                .is_some_and(|targets| targets.contains(&address))
                && receipt.delivery_id == format!("{message_id}:{}", address_key(&address)),
            "receipt does not belong to this worker"
        );
        if record.receipts.get(&receipt.delivery_id) == Some(&receipt) {
            return Ok(());
        }
        record
            .receipts
            .insert(receipt.delivery_id.clone(), receipt.clone());
        self.store.put("message", message_id, &record).await?;
        self.notify();
        Ok(())
    }
}
