use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::RejectedOutbox;
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
                if let Err(error) = self.accept_outbox(machine, server, &entry).await {
                    // Preserve the rejected envelope before acknowledging it.
                    // One malformed entry must not strand later valid mail.
                    let id = format!("{}/{}/{}", machine.id, server.id, entry.sequence);
                    self.store
                        .put(
                            "rejected_outbox",
                            &id,
                            &RejectedOutbox {
                                machine_id: machine.id.clone(),
                                server_id: server.id.clone(),
                                entry: entry.clone(),
                                reason: error.to_string(),
                            },
                        )
                        .await?;
                    self.notify();
                }
                sequences.push(entry.sequence);
            }
            // The coordinator's writes are committed before acknowledging the
            // host outbox. Lost acknowledgements only replay idempotent records.
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

    pub(crate) async fn accept_outbox(
        &self,
        machine: &Machine,
        server: &Server,
        entry: &TeamOutboxEntry,
    ) -> Result<()> {
        match entry.kind.as_str() {
            "message" => {
                let outbound: OutboundMessage = serde_json::from_value(entry.payload.clone())?;
                ensure!(
                    outbound.message.sender.machine_id == machine.id
                        && outbound.message.sender.server_id == server.id,
                    "message sender belongs to another worker"
                );
                self.ingest(outbound.message, outbound.delivery).await?;
            }
            "subscription" => {
                let subscription: Subscription = serde_json::from_value(entry.payload.clone())?;
                ensure!(
                    subscription.sender.machine_id == machine.id
                        && subscription.sender.server_id == server.id,
                    "subscriber belongs to another worker"
                );
                self.subscribe(
                    subscription.sender,
                    subscription.topic,
                    subscription.enabled,
                )
                .await?;
            }
            "receipt" => {
                self.receipt(
                    &machine.id,
                    &server.id,
                    serde_json::from_value(entry.payload.clone())?,
                )
                .await?
            }
            kind => anyhow::bail!("unknown host outbox entry kind: {kind}"),
        }
        Ok(())
    }
}
