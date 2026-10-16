use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamDeliverParams;
use codex_app_server_protocol::TeamDeliverResponse;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamReceiptState;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::turn_input::TeamMessageInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputSubmission;
use uuid::Uuid;

use crate::TeamHostService;
use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::DeliveryAttempt;
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
    pub async fn deliver(&self, mut params: TeamDeliverParams) -> Result<TeamDeliverResponse> {
        params.delivery.thread_id = ThreadId::from_string(&params.delivery.thread_id)?.to_string();
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes dispatch intent and receipt acknowledgements with lifecycle evidence"
    )]
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
            let submission_id = receipt
                .turn_id
                .clone()
                .unwrap_or_else(|| Uuid::now_v7().to_string());
            receipt.state = TeamReceiptState::NeedsReconciliation;
            receipt.turn_id = Some(match &mode {
                TurnInputMode::Steer { expected_turn_id } => expected_turn_id.clone(),
                TurnInputMode::StartIfIdle
                | TurnInputMode::StartOrSteer
                | TurnInputMode::ContinueIfIdle { .. } => submission_id.clone(),
            });
            let update = self.delivery_updates.lock().await;
            store.update_receipt(&receipt).await?;
            store
                .put(
                    "delivery_attempt",
                    &delivery.id,
                    &DeliveryAttempt {
                        id: Uuid::now_v7().to_string(),
                        generation: history.generation,
                        admission_revision: _permit
                            .revision()
                            .context("enrolled dispatch admission has no revision")?,
                        turn_id: receipt
                            .turn_id
                            .clone()
                            .context("delivery dispatch has no turn identity")?,
                        acknowledged: false,
                        participation: Default::default(),
                        provider_acknowledgements: Default::default(),
                        terminal_outcomes: Default::default(),
                    },
                )
                .await?;
            drop(update);
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
            let _update = self.delivery_updates.lock().await;
            let mut finished_before_ack = false;
            if let Some(mut attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &receipt.delivery_id)
                .await?
            {
                finished_before_ack = !attempt.terminal_outcomes.is_empty();
                attempt.acknowledged = !matches!(result, TurnInputSubmission::NotSubmitted { .. });
                store
                    .put("delivery_attempt", &receipt.delivery_id, &attempt)
                    .await?;
            }
            match result {
                TurnInputSubmission::Started { turn_id, .. }
                | TurnInputSubmission::Steered { turn_id, .. } => {
                    receipt.state = TeamReceiptState::Accepted;
                    receipt.turn_id = Some(turn_id);
                }
                TurnInputSubmission::NotSubmitted { .. } => {
                    receipt.state = TeamReceiptState::Pending;
                }
            }
            let latest = store
                .deliveries(&receipt.thread_id)
                .await?
                .into_iter()
                .find(|(_, latest)| latest.delivery_id == receipt.delivery_id)
                .map(|(_, receipt)| receipt);
            if !finished_before_ack
                && !latest.is_some_and(|latest| {
                    matches!(
                        latest.state,
                        TeamReceiptState::Completed | TeamReceiptState::Interrupted
                    )
                })
            {
                store.update_receipt(&receipt).await?;
            }
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes receipt reconciliation with concurrent dispatch acknowledgements"
    )]
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
        let _update = self.delivery_updates.lock().await;
        for (delivery, mut receipt) in store.deliveries(thread_id).await? {
            if !matches!(
                receipt.state,
                TeamReceiptState::Accepted
                    | TeamReceiptState::Interrupted
                    | TeamReceiptState::NeedsReconciliation
            ) {
                continue;
            }
            let Some(attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
                .await?
            else {
                receipt.state = TeamReceiptState::NeedsReconciliation;
                receipt.detail = Some(
                    "Legacy delivery has no dispatch or model-request participation evidence."
                        .into(),
                );
                store.update_receipt(&receipt).await?;
                continue;
            };
            if attempt.generation != history.generation {
                receipt.state = TeamReceiptState::NeedsReconciliation;
                receipt.detail = Some(
                    "Thread history changed; inspect the original delivery before continuing."
                        .into(),
                );
            } else if attempt
                .participation
                .values()
                .any(|outcome| *outcome == Some(AttemptOutcome::Completed))
            {
                receipt.state = TeamReceiptState::Completed;
            } else if thread.team_active_turn_id().await.as_ref() == receipt.turn_id.as_ref()
                && receipt.turn_id.is_some()
            {
                receipt.state = TeamReceiptState::Accepted;
            } else {
                let marker = format!("msg_team-{}", delivery.id);
                let recorded = history.items.iter().any(|item| matches!(item,
                    RolloutItem::ResponseItem(item) if matches!(&item.item, ResponseItem::Message { id: Some(id), .. } if id.as_str() == marker)));
                let stopped = store
                    .get::<i64>("stop_settled_revision", thread_id)
                    .await?
                    .is_some_and(|revision| {
                        u64::try_from(revision)
                            .is_ok_and(|revision| revision > attempt.admission_revision)
                    });
                if attempt.acknowledged && attempt.participation.is_empty() && !recorded && stopped
                {
                    // Cancellation has settled and no live queue or history
                    // copy remains. Only this proven unsampled input may retry.
                    receipt.state = TeamReceiptState::Pending;
                    receipt.turn_id = None;
                    receipt.detail = None;
                } else {
                    receipt.state = TeamReceiptState::NeedsReconciliation;
                    receipt.detail = Some("Dispatch or model outcome is uncertain; inspect the original attempt before continuing.".into());
                }
            }
            store.update_receipt(&receipt).await?;
        }
        Ok(())
    }
}
