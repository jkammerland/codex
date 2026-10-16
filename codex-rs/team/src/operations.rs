//! Logical submissions retain their identity across a lost HTTP response.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::model::*;

impl Coordinator {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "mutation guards serialize durable deduplication and effects for subscription, control and retry operations"
    )]
    pub(crate) async fn execute_operation(&self, operation: TeamOperation) -> Result<TeamResponse> {
        if matches!(
            &operation.request,
            TeamRequest::Snapshot
                | TeamRequest::Models { .. }
                | TeamRequest::GiteaInspect { .. }
                | TeamRequest::AgentCredential { .. }
        ) {
            return self.execute(operation.request).await;
        }
        let id = Uuid::parse_str(&operation.operation_id)?.to_string();
        let digest = serde_json::to_string(&format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&operation.request)?)
        ))?;
        if let Some(response) = self.operation_result(&id, &digest).await? {
            return Ok(response);
        }
        match operation.request {
            TeamRequest::Send {
                recipient,
                body,
                kind,
                reply_to,
                delivery,
            } => {
                ensure!(
                    kind != TeamMessageKind::Board,
                    "use post for board messages"
                );
                let mut message = self
                    .operator_message(Some(recipient), body, kind, reply_to, None)
                    .await?;
                message.id = id.clone();
                self.commit_message_operation(&id, &digest, message, delivery)
                    .await
            }
            TeamRequest::Post { topic, body } => {
                let mut message = self
                    .operator_message(None, body, TeamMessageKind::Board, None, Some(topic))
                    .await?;
                message.id = id.clone();
                self.commit_message_operation(&id, &digest, message, TeamDeliveryMode::NextTurn)
                    .await
            }
            TeamRequest::Reply {
                message_id,
                body,
                delivery,
            } => {
                let original = self
                    .store
                    .get::<MessageRecord>("message", &message_id)
                    .await?
                    .context("original message is missing")?
                    .message;
                let board = original.kind == TeamMessageKind::Board;
                let mut message = self
                    .operator_message(
                        if board { None } else { Some(original.sender) },
                        body,
                        if board {
                            TeamMessageKind::Board
                        } else {
                            TeamMessageKind::Reply
                        },
                        Some(message_id),
                        original.topic,
                    )
                    .await?;
                message.id = id.clone();
                self.commit_message_operation(&id, &digest, message, delivery)
                    .await
            }
            TeamRequest::Subscribe {
                address,
                topic,
                enabled,
            } => {
                let _mutation = self.mutations.lock().await;
                if let Some(response) = self.operation_result(&id, &digest).await? {
                    return Ok(response);
                }
                let (key, subscription) =
                    self.prepare_subscription(address, topic, enabled).await?;
                self.store
                    .put_record_once(
                        "operator_operation",
                        &id,
                        &digest,
                        &[
                            ("subscription", &key, serde_json::to_value(subscription)?),
                            (
                                "operator_result",
                                &id,
                                serde_json::to_value(TeamResponse::Done)?,
                            ),
                        ],
                    )
                    .await?;
                self.notify();
                Ok(TeamResponse::Done)
            }
            TeamRequest::Control { mode } => {
                let _mutation = self.mutations.lock().await;
                if let Some(response) = self.operation_result(&id, &digest).await? {
                    return Ok(response);
                }
                let mut control = self.current_control().await?;
                control.revision = control
                    .revision
                    .checked_add(1)
                    .context("control revision exhausted")?;
                control.mode = mode;
                let response = TeamResponse::Control {
                    control: control.clone(),
                };
                self.store
                    .apply_control_once(&control, &id, &digest, &serde_json::to_value(&response)?)
                    .await?;
                self.notify();
                Ok(response)
            }
            TeamRequest::SetModel {
                address,
                model,
                effort,
            } => Ok(TeamResponse::Agent {
                agent: Box::new(
                    self.select_model_operation(
                        address,
                        TeamSelection::Value { value: model },
                        effort.map_or(TeamSelection::Default, |value| TeamSelection::Value {
                            value,
                        }),
                        Some((&id, &digest)),
                    )
                    .await?,
                ),
            }),
            TeamRequest::SelectModel {
                address,
                model,
                effort,
            } => Ok(TeamResponse::Agent {
                agent: Box::new(
                    self.select_model_operation(address, model, effort, Some((&id, &digest)))
                        .await?,
                ),
            }),
            TeamRequest::Enroll { address, alias } => {
                let response = TeamResponse::Agent {
                    agent: Box::new(self.enroll(address, alias, Some(&id)).await?),
                };
                self.store
                    .put_record_once(
                        "operator_operation",
                        &id,
                        &digest,
                        &[("operator_result", &id, serde_json::to_value(&response)?)],
                    )
                    .await?;
                Ok(response)
            }
            TeamRequest::Leave { address } => {
                self.leave(address, Some(&id)).await?;
                self.store
                    .put_record_once(
                        "operator_operation",
                        &id,
                        &digest,
                        &[(
                            "operator_result",
                            &id,
                            serde_json::to_value(TeamResponse::Done)?,
                        )],
                    )
                    .await?;
                Ok(TeamResponse::Done)
            }
            TeamRequest::Retry { message_id } => {
                let _mutation = self.mutations.lock().await;
                if let Some(response) = self.operation_result(&id, &digest).await? {
                    return Ok(response);
                }
                let mut record = self
                    .store
                    .get::<MessageRecord>("message", &message_id)
                    .await?
                    .context("message not found")?;
                record.error = None;
                let response = TeamResponse::Message {
                    message: Box::new(record.clone()),
                };
                self.store
                    .put_record_once(
                        "operator_operation",
                        &id,
                        &digest,
                        &[
                            ("gitea_retry", &message_id, serde_json::to_value(true)?),
                            ("message", &message_id, serde_json::to_value(&record)?),
                            ("operator_result", &id, serde_json::to_value(&response)?),
                        ],
                    )
                    .await?;
                self.notify();
                Ok(response)
            }
            TeamRequest::Snapshot
            | TeamRequest::Models { .. }
            | TeamRequest::GiteaInspect { .. }
            | TeamRequest::AgentCredential { .. } => {
                unreachable!("read-only operations returned above")
            }
        }
    }

    pub(crate) async fn operation_result(
        &self,
        id: &str,
        digest: &str,
    ) -> Result<Option<TeamResponse>> {
        if let Some(previous) = self.store.get::<String>("operator_operation", id).await? {
            ensure!(
                serde_json::to_string(&previous)? == digest,
                "operation identity has conflicting content"
            );
            return Ok(Some(
                self.store
                    .get("operator_result", id)
                    .await?
                    .context("operation result is missing")?,
            ));
        }
        Ok(None)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "message validation, operation deduplication and durable publication must observe one mutation order"
    )]
    async fn commit_message_operation(
        &self,
        id: &str,
        digest: &str,
        message: TeamMessage,
        delivery: TeamDeliveryMode,
    ) -> Result<TeamResponse> {
        let _mutation = self.mutations.lock().await;
        if let Some(response) = self.operation_result(id, digest).await? {
            return Ok(response);
        }
        let record = self.prepare_message(message, delivery).await?;
        let response = TeamResponse::Message {
            message: Box::new(record.clone()),
        };
        self.store
            .put_record_once(
                "operator_operation",
                id,
                digest,
                &[
                    (
                        "message",
                        &record.message.id,
                        serde_json::to_value(&record)?,
                    ),
                    ("operator_result", id, serde_json::to_value(&response)?),
                ],
            )
            .await?;
        self.notify();
        Ok(response)
    }
}
