use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use sha2::Digest;
use sha2::Sha256;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::RejectedOutbox;
use crate::model::server_key;
use crate::routing::OutboundMessage;
use crate::routing::Subscription;

impl Coordinator {
    pub(crate) async fn drain_outbox(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        let coordinator_id = self.current_control().await?.coordinator_id;
        loop {
            let outbox: TeamOutboxListResponse = peer
                .handle
                .request_typed(ClientRequest::TeamOutboxList {
                    request_id: request_id(),
                    params: TeamOutboxListParams {
                        coordinator_id: coordinator_id.clone(),
                        cursor: None,
                        limit: Some(100),
                    },
                })
                .await?;
            if outbox.data.is_empty() {
                break;
            }
            let mut sequences = Vec::new();
            for entry in outbox.data {
                self.accept_outbox(machine, server, &entry).await?;
                sequences.push(entry.sequence);
            }
            // Effects and intake identity are committed together before ACK.
            // Replayed subscriptions cannot overwrite newer unsubscribe state.
            let _: TeamOutboxAckResponse = peer
                .handle
                .request_typed(ClientRequest::TeamOutboxAck {
                    request_id: request_id(),
                    params: TeamOutboxAckParams {
                        coordinator_id: coordinator_id.clone(),
                        sequences,
                    },
                })
                .await?;
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "deduplication, effect preparation and durable intake commit must be ordered against other message and subscription mutations"
    )]
    pub(crate) async fn accept_outbox(
        &self,
        machine: &Machine,
        server: &Server,
        entry: &TeamOutboxEntry,
    ) -> Result<()> {
        let installation = self
            .store
            .get::<String>("installation", &server_key(&machine.id, &server.id))
            .await?
            .context("worker installation is not bound")?;
        let id = format!("{installation}/{}", entry.id);
        let digest =
            serde_json::to_string(&format!("{:x}", Sha256::digest(serde_json::to_vec(entry)?)))?;
        let _mutation = self.mutations.lock().await;
        if let Some(previous) = self.store.get::<String>("outbox_intake", &id).await? {
            ensure!(
                serde_json::to_string(&previous)? == digest,
                "outbox identity has conflicting content"
            );
            return Ok(());
        }
        let prepared: Result<(&str, String, serde_json::Value)> = async {
            match entry.kind.as_str() {
                "message" => {
                    let outbound: OutboundMessage = serde_json::from_value(entry.payload.clone())?;
                    ensure!(
                        outbound.message.sender.machine_id == machine.id
                            && outbound.message.sender.server_id == server.id,
                        "message sender belongs to another worker"
                    );
                    let record = self
                        .prepare_message(outbound.message, outbound.delivery)
                        .await?;
                    Ok((
                        "message",
                        record.message.id.clone(),
                        serde_json::to_value(record)?,
                    ))
                }
                "subscription" => {
                    let subscription: Subscription = serde_json::from_value(entry.payload.clone())?;
                    ensure!(
                        subscription.sender.machine_id == machine.id
                            && subscription.sender.server_id == server.id,
                        "subscriber belongs to another worker"
                    );
                    let (key, subscription) = self
                        .prepare_subscription(
                            subscription.sender,
                            subscription.topic,
                            subscription.enabled,
                        )
                        .await?;
                    Ok(("subscription", key, serde_json::to_value(subscription)?))
                }
                "receipt" => {
                    let (key, record) = self
                        .prepare_receipt(
                            &machine.id,
                            &server.id,
                            serde_json::from_value(entry.payload.clone())?,
                        )
                        .await?;
                    Ok(("message", key, serde_json::to_value(record)?))
                }
                kind => anyhow::bail!("unknown host outbox entry kind: {kind}"),
            }
        }
        .await;
        let (namespace, key, value) = match prepared {
            Ok(effect) => effect,
            Err(error) => (
                "rejected_outbox",
                id.clone(),
                serde_json::to_value(RejectedOutbox {
                    machine_id: machine.id.clone(),
                    server_id: server.id.clone(),
                    entry: entry.clone(),
                    reason: error.to_string(),
                })?,
            ),
        };
        self.store
            .put_record_once("outbox_intake", &id, &digest, &[(namespace, &key, value)])
            .await?;
        self.notify();
        Ok(())
    }
}
