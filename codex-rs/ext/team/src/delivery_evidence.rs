//! Delivery dispatch and model participation are separate durable facts.

use std::collections::BTreeMap;

use anyhow::Result;
use codex_app_server_protocol::TeamDeliveryAbandonment;
use codex_app_server_protocol::TeamReceiptState;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnModelRequestInput;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::TeamHostService;

pub(crate) struct RuntimeAttempt(pub String);

pub(crate) fn runtime_attempt(store: &ExtensionData) -> String {
    store
        .get_or_init(|| RuntimeAttempt(Uuid::now_v7().to_string()))
        .0
        .clone()
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryAttempt {
    pub id: String,
    pub generation: String,
    pub admission_revision: u64,
    pub turn_id: String,
    pub acknowledged: bool,
    pub participation: BTreeMap<String, Option<AttemptOutcome>>,
    #[serde(default)]
    pub provider_acknowledgements: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub terminal_outcomes: BTreeMap<String, AttemptOutcome>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum AttemptOutcome {
    Completed,
    Interrupted,
    Failed(String),
}

impl TeamHostService {
    #[allow(clippy::await_holding_invalid_type)]
    pub(crate) async fn journal_model_request(
        &self,
        input: TurnModelRequestInput<'_>,
    ) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let runtime = runtime_attempt(input.turn_store);
        let _update = self.delivery_updates.lock().await;
        for (delivery, _) in store.deliveries(input.thread_store.level_id()).await? {
            if store
                .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                .await?
                .is_some()
            {
                continue;
            }
            let marker = format!("msg_team-{}", delivery.id);
            if !input
                .input
                .iter()
                .any(|item| item.id().is_some_and(|id| id.as_str() == marker))
            {
                continue;
            }
            let Some(mut attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
                .await?
            else {
                // Legacy history has no dispatch proof. It may remain in the
                // prompt, but must never manufacture a completed receipt.
                continue;
            };
            attempt.participation.entry(runtime.clone()).or_insert(None);
            store
                .put("delivery_attempt", &delivery.id, &attempt)
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::await_holding_invalid_type)]
    pub(crate) async fn journal_provider_acknowledgement(
        &self,
        input: codex_extension_api::TurnModelAcknowledgedInput<'_>,
    ) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let runtime = runtime_attempt(input.turn_store);
        let _update = self.delivery_updates.lock().await;
        for (delivery, _) in store.deliveries(input.thread_store.level_id()).await? {
            if store
                .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                .await?
                .is_some()
            {
                continue;
            }
            if let Some(mut attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
                .await?
                && attempt.participation.contains_key(&runtime)
            {
                attempt
                    .provider_acknowledgements
                    .insert(runtime.clone(), input.response_id.map(str::to_owned));
                store
                    .put("delivery_attempt", &delivery.id, &attempt)
                    .await?;
            }
        }
        Ok(())
    }

    #[allow(clippy::await_holding_invalid_type)]
    pub(crate) async fn finish_deliveries(
        &self,
        thread_id: &str,
        turn_id: &str,
        runtime: &str,
        outcome: AttemptOutcome,
    ) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let _update = self.delivery_updates.lock().await;
        for (delivery, mut receipt) in store.deliveries(thread_id).await? {
            if store
                .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                .await?
                .is_some()
            {
                continue;
            }
            let Some(mut attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
                .await?
            else {
                if receipt.turn_id.as_deref() == Some(turn_id)
                    && receipt.state != TeamReceiptState::Completed
                {
                    receipt.state = TeamReceiptState::NeedsReconciliation;
                    receipt.detail =
                        Some("Legacy delivery has no model-request participation evidence.".into());
                    store.update_receipt(&receipt).await?;
                }
                continue;
            };
            if !attempt.participation.contains_key(runtime) {
                // Turn IDs do not establish ownership: a later turn may sample
                // the original marker, while a runtime sharing its turn may not.
                // Only request participation can establish an outcome.
                continue;
            }
            attempt
                .terminal_outcomes
                .insert(runtime.to_owned(), outcome.clone());
            attempt
                .participation
                .insert(runtime.to_owned(), Some(outcome.clone()));
            store
                .put("delivery_attempt", &delivery.id, &attempt)
                .await?;
            if receipt.state == TeamReceiptState::Completed {
                continue;
            }
            receipt.state = match &outcome {
                AttemptOutcome::Completed => TeamReceiptState::Completed,
                AttemptOutcome::Failed(_) => TeamReceiptState::NeedsReconciliation,
                AttemptOutcome::Interrupted => TeamReceiptState::Interrupted,
            };
            receipt.detail = match &outcome {
                AttemptOutcome::Failed(error) => {
                    Some(format!("Participating turn failed: {error}"))
                }
                AttemptOutcome::Completed | AttemptOutcome::Interrupted => None,
            };
            store.update_receipt(&receipt).await?;
        }
        Ok(())
    }
}
