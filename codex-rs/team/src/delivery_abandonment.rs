//! Operator dispositions retain processing evidence and are published by the owning host.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::coordinator::request_id;
use crate::model::*;

impl Coordinator {
    pub(crate) async fn abandon_delivery(
        &self,
        address: TeamAddress,
        message_id: String,
        reason: String,
        operation_id: &str,
        digest: &str,
    ) -> Result<TeamResponse> {
        let address = canonical_address(address)?;
        ensure!(
            !reason.trim().is_empty(),
            "an abandonment reason is required"
        );
        let delivery_id = format!("{message_id}:{}", address_key(&address));
        let params = TeamDeliveryAbandonParams {
            operation_id: operation_id.into(),
            coordinator_id: self.current_control().await?.coordinator_id,
            thread_id: address.thread_id.clone(),
            delivery_id,
            reason,
        };

        // Retain the exact request before contacting the worker. A lost reply
        // must retry the same disposition after retirement has already finished.
        {
            let _mutation = self.mutations.lock().await;
            if let Some(response) = self.operation_result(operation_id, digest).await? {
                return Ok(response);
            }
            if let Some(previous) = self
                .store
                .get::<TeamDeliveryAbandonParams>("delivery_abandon_request", operation_id)
                .await?
            {
                ensure!(
                    previous == params,
                    "operation identity has conflicting content"
                );
            } else {
                let record = self
                    .store
                    .get::<MessageRecord>("message", &message_id)
                    .await?
                    .context("message not found")?;
                ensure!(
                    record
                        .targets
                        .as_ref()
                        .is_some_and(|targets| targets.contains(&address)),
                    "message was not addressed to this member"
                );
                self.store
                    .put_record_once(
                        "operator_operation",
                        operation_id,
                        digest,
                        &[(
                            "delivery_abandon_request",
                            operation_id,
                            serde_json::to_value(&params)?,
                        )],
                    )
                    .await?;
            }
        }
        let peer = self.peer(&address.machine_id, &address.server_id).await?;
        let response: TeamDeliveryAbandonResponse = peer
            .handle
            .request_typed(ClientRequest::TeamDeliveryAbandon {
                request_id: request_id(),
                params: params.clone(),
            })
            .await?;
        ensure!(
            response.abandonment
                == TeamDeliveryAbandonment {
                    operation_id: params.operation_id,
                    delivery_id: params.delivery_id,
                    thread_id: params.thread_id,
                    reason: params.reason,
                },
            "worker returned a different delivery disposition"
        );

        let _mutation = self.mutations.lock().await;
        if let Some(response) = self.operation_result(operation_id, digest).await? {
            return Ok(response);
        }
        let writes = self
            .prepare_delivery_abandonment(
                &address.machine_id,
                &address.server_id,
                &response.abandonment,
            )
            .await?;
        let result = TeamResponse::DeliveryAbandoned {
            abandonment: response.abandonment,
        };
        let mut writes = writes;
        writes.push((
            "operator_result",
            operation_id.into(),
            serde_json::to_value(&result)?,
        ));
        self.store
            .put_records(&crate::board::borrowed_writes(&writes))
            .await?;
        self.notify();
        Ok(result)
    }

    pub(crate) async fn prepare_delivery_abandonment(
        &self,
        machine: &str,
        server: &str,
        abandonment: &TeamDeliveryAbandonment,
    ) -> Result<crate::board::BoardWrites> {
        ensure!(
            !abandonment.reason.trim().is_empty(),
            "an abandonment reason is required"
        );
        uuid::Uuid::parse_str(&abandonment.operation_id)?;
        let address = canonical_address(TeamAddress {
            machine_id: machine.into(),
            server_id: server.into(),
            thread_id: abandonment.thread_id.clone(),
        })?;
        let suffix = format!(":{}", address_key(&address));
        let message_id = abandonment
            .delivery_id
            .strip_suffix(&suffix)
            .context("delivery belongs to another worker or thread")?;
        let mut record = self
            .store
            .get::<MessageRecord>("message", message_id)
            .await?
            .context("message not found")?;
        ensure!(
            record
                .targets
                .as_ref()
                .is_some_and(|targets| targets.contains(&address)),
            "delivery recipient is not a message target"
        );
        if let Some(previous) = record.abandonments.get(&abandonment.delivery_id) {
            ensure!(
                previous == abandonment,
                "delivery already has a different abandonment"
            );
        }
        record
            .abandonments
            .insert(abandonment.delivery_id.clone(), abandonment.clone());
        Ok(vec![(
            "message",
            message_id.into(),
            serde_json::to_value(record)?,
        )])
    }
}

#[cfg(test)]
#[path = "delivery_abandonment_tests.rs"]
mod tests;
