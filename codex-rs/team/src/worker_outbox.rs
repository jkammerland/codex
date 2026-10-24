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
        let prepared: Result<crate::board::BoardWrites> = async {
            match entry.kind.as_str() {
                "delivery_abandonment" => {
                    let abandonment: TeamDeliveryAbandonment =
                        serde_json::from_value(entry.payload.clone())?;
                    self.prepare_delivery_abandonment(&machine.id, &server.id, &abandonment)
                        .await
                }
                "administration" => {
                    let operation: TeamAdministrationOperation =
                        serde_json::from_value(entry.payload.clone())?;
                    ensure!(
                        operation.operation_id == entry.id
                            && operation.actor.machine_id == machine.id
                            && operation.actor.server_id == server.id,
                        "administration actor belongs to another worker"
                    );
                    self.prepare_administration(operation).await
                }
                "message" => {
                    let outbound: OutboundMessage = serde_json::from_value(entry.payload.clone())?;
                    ensure!(
                        outbound.message.sender.machine_id == machine.id
                            && outbound.message.sender.server_id == server.id,
                        "message sender belongs to another worker"
                    );
                    let (_, writes) = self
                        .prepare_message_effects(outbound.message, outbound.delivery, Vec::new())
                        .await?;
                    Ok(writes)
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
                    self.subscription_effects(key, subscription).await
                }
                "board" => {
                    #[derive(serde::Deserialize)]
                    struct BoardOutbox {
                        sender: TeamAddress,
                        request: TeamBoardParams,
                    }
                    let outbound: BoardOutbox = serde_json::from_value(entry.payload.clone())?;
                    ensure!(
                        outbound.sender.machine_id == machine.id
                            && outbound.sender.server_id == server.id,
                        "board sender belongs to another worker"
                    );
                    let identity = TeamBoardIdentity::Agent {
                        address: crate::model::canonical_address(outbound.sender)?,
                    };
                    self.board_author(&identity, false).await?;
                    let (_, writes) = self
                        .prepare_board_request(outbound.request, identity, &entry.id)
                        .await?;
                    Ok(writes)
                }
                "receipt" => {
                    let (key, record) = self
                        .prepare_receipt(
                            &machine.id,
                            &server.id,
                            serde_json::from_value(entry.payload.clone())?,
                        )
                        .await?;
                    Ok(vec![("message", key, serde_json::to_value(record)?)])
                }
                kind => anyhow::bail!("unknown host outbox entry kind: {kind}"),
            }
        }
        .await;
        let writes = match prepared {
            Ok(effects) => effects,
            Err(error) => {
                // The store scrubs removed replies and any previously rejected
                // descendants together with the durable intake receipt.
                let mut writes = vec![(
                    "rejected_outbox",
                    id.clone(),
                    serde_json::to_value(RejectedOutbox {
                        machine_id: machine.id.clone(),
                        server_id: server.id.clone(),
                        entry: entry.clone(),
                        reason: error.to_string(),
                    })?,
                )];
                // A valid host-origin operation still gets terminal status when
                // its actor was removed before coordinator intake.
                if entry.kind == "administration"
                    && let Ok(mut operation) =
                        serde_json::from_value::<TeamAdministrationOperation>(entry.payload.clone())
                    && operation.operation_id == entry.id
                    && operation.actor.machine_id == machine.id
                    && operation.actor.server_id == server.id
                    && uuid::Uuid::parse_str(&operation.operation_id).is_ok()
                    && self
                        .store
                        .get::<TeamAdministrationOperation>(
                            "administration",
                            &operation.operation_id,
                        )
                        .await?
                        .is_none()
                {
                    operation.state = TeamAdministrationState::Rejected;
                    operation.waiting_reason = Some(error.to_string());
                    operation.result = None;
                    writes.push((
                        "administration",
                        operation.operation_id.clone(),
                        serde_json::to_value(operation)?,
                    ));
                }
                writes
            }
        };
        self.store
            .put_outbox_intake(&id, &digest, &crate::board::borrowed_writes(&writes))
            .await?;
        self.notify();
        Ok(())
    }
}
