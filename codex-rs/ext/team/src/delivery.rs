use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamDeliverParams;
use codex_app_server_protocol::TeamDeliverResponse;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamReceiptState;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::TeamMessageInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputSubmission;
use uuid::Uuid;

use crate::TeamHostService;
use crate::lifecycle::HostEvent;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryBoundary<'a> {
    TurnStart { turn_id: &'a str },
    IdleOrSafeBoundary,
}

impl TeamHostService {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub async fn deliver(&self, params: TeamDeliverParams) -> Result<TeamDeliverResponse> {
        let _operation = self.operations.lock().await;
        let store = self.require_coordinator(&params.coordinator_id).await?;
        ensure!(
            self.gate
                .root(ThreadId::from_string(&params.delivery.thread_id)?)
                .is_some(),
            "recipient is not enrolled"
        );
        let receipt = store.receive(&params.delivery).await?;
        let _ = self.events.send(HostEvent::Dispatch);
        self.notify();
        Ok(TeamDeliverResponse { receipt })
    }

    pub(crate) async fn dispatch_delivery(
        &self,
        thread_id: ThreadId,
        boundary: DeliveryBoundary<'_>,
    ) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let Ok(thread) = manager.get_thread(thread_id).await else {
            return Ok(());
        };
        for (delivery, mut receipt) in store.deliveries(&thread_id.to_string()).await? {
            if receipt.state != TeamReceiptState::Pending {
                continue;
            }
            let active = thread.team_active_turn_id().await;
            let mode = match (boundary, active, delivery.mode, delivery.wake) {
                // The lifecycle callback precedes installing the active task.
                // The input loop processes this after startup; use its known
                // turn ID rather than mistaking that brief window for idle.
                (DeliveryBoundary::TurnStart { turn_id }, _, _, _) => TurnInputMode::Steer {
                    expected_turn_id: turn_id.to_owned(),
                },
                (_, Some(expected_turn_id), TeamDeliveryMode::SafeBoundary, _) => {
                    TurnInputMode::Steer { expected_turn_id }
                }
                (_, None, _, true) => TurnInputMode::StartIfIdle,
                _ => continue,
            };
            let Some(_permit) = self.gate.try_enter(thread_id) else {
                return Ok(());
            };
            if matches!(mode, TurnInputMode::StartIfIdle)
                && let Some(error) = thread.team_model_error().await
            {
                receipt.detail = Some(error);
                store.update_receipt(&receipt).await?;
                continue;
            }
            receipt.detail = None;
            let history = thread.team_execution_history().await?;
            store
                .put("delivery_generation", &delivery.id, &history.generation)
                .await?;
            let submission_id = receipt
                .turn_id
                .clone()
                .unwrap_or_else(|| Uuid::now_v7().to_string());
            receipt.state = TeamReceiptState::NeedsReconciliation;
            receipt.turn_id = Some(match &mode {
                TurnInputMode::Steer { expected_turn_id } => expected_turn_id.clone(),
                TurnInputMode::StartIfIdle | TurnInputMode::StartOrSteer => submission_id.clone(),
            });
            store.update_receipt(&receipt).await?;
            let result = thread
                .deliver_team_message(
                    TeamMessageInput {
                        message_id: delivery.message.id,
                        delivery_id: delivery.id,
                        sender: serde_json::to_string(&delivery.message.sender)?,
                        body: delivery.message.body,
                        reply_to: delivery.message.reply_to,
                    },
                    submission_id,
                    mode,
                )
                .await?;
            match result {
                TurnInputSubmission::Started { turn_id }
                | TurnInputSubmission::Steered { turn_id } => {
                    receipt.state = TeamReceiptState::Accepted;
                    receipt.turn_id = Some(turn_id);
                }
                TurnInputSubmission::NotSubmitted { .. } => {
                    receipt.state = TeamReceiptState::Pending;
                }
            }
            store.update_receipt(&receipt).await?;
        }
        Ok(())
    }

    pub(crate) async fn reconcile_delivery(&self, thread_id: &str) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        if self.gate.root(ThreadId::from_string(thread_id)?).is_none() {
            return Ok(());
        }
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let thread = manager
            .get_thread(ThreadId::from_string(thread_id)?)
            .await?;
        let history = thread.team_execution_history().await?;
        for (delivery, mut receipt) in store.deliveries(thread_id).await? {
            if !matches!(
                receipt.state,
                TeamReceiptState::Accepted | TeamReceiptState::NeedsReconciliation
            ) {
                continue;
            }
            if store
                .get::<String>("delivery_generation", &delivery.id)
                .await?
                .as_ref()
                != Some(&history.generation)
            {
                receipt.state = TeamReceiptState::NeedsReconciliation;
                receipt.detail = Some(
                    "Thread history changed; inspect the original delivery before continuing."
                        .into(),
                );
                store.update_receipt(&receipt).await?;
                continue;
            }
            let marker = format!("msg_team-{}", delivery.id);
            let recorded = history.items.iter().any(|item| matches!(item,
                RolloutItem::ResponseItem(item) if matches!(&item.item, ResponseItem::Message { id: Some(id), .. } if id.as_str() == marker)));
            let completed = history.items.iter().any(|item| matches!(item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if receipt.turn_id.as_deref() == Some(&event.turn_id)));
            if recorded && completed {
                receipt.state = TeamReceiptState::Completed;
            } else if thread.team_active_turn_id().await.as_ref() == receipt.turn_id.as_ref()
                && receipt.turn_id.is_some()
            {
                receipt.state = TeamReceiptState::Accepted;
            } else if !recorded {
                // Enrolled sessions flush input before sampling, so absence of
                // the marker proves this message did not drive model execution.
                receipt.state = TeamReceiptState::Pending;
            } else {
                receipt.state = TeamReceiptState::NeedsReconciliation;
                receipt.detail =
                    Some("Input is durable; inspect the original turn before continuing.".into());
            }
            store.update_receipt(&receipt).await?;
        }
        Ok(())
    }
}
