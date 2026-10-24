//! Explicit operator recovery for unresolved historical retirement evidence.

use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use uuid::Uuid;

use crate::TeamHostService;
use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::DeliveryAttempt;
use crate::lifecycle::HostEvent;

impl TeamHostService {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "recovery holds family admission and delivery evidence through its durable commit"
    )]
    pub async fn abandon_delivery(
        &self,
        mut params: TeamDeliveryAbandonParams,
    ) -> Result<TeamDeliveryAbandonResponse> {
        ensure!(
            Uuid::parse_str(&params.operation_id)?.to_string() == params.operation_id,
            "operation identity must be a canonical UUID"
        );
        ensure!(
            !params.reason.trim().is_empty(),
            "an abandonment reason is required"
        );
        let thread_id = ThreadId::from_string(&params.thread_id)?;
        params.thread_id = thread_id.to_string();
        let operation_key = format!("{}:{}", params.coordinator_id, params.operation_id);
        let digest = serde_json::to_string(&params)?;
        let _operation = self.operations.lock().await;
        let store = self.store.get().context("team host is not enrolled")?;

        // Removal may already have revoked enrollment and detached the gate.
        // Recover the original result before requiring either boundary again.
        if let Some(response) = store.host_operation_result(&operation_key, &digest).await? {
            return Ok(response);
        }
        self.require_coordinator(&params.coordinator_id).await?;
        let model_update = self.model_updates.lock().await;
        let root = self
            .gate
            .root(thread_id)
            .context("recipient is not enrolled")?;
        let retirement = store
            .get::<TeamMemberRetirement>("retirement", &root.to_string())
            .await?
            .context("recipient family is not retiring")?;
        ensure!(
            retirement.state == TeamMemberRetirementState::Retiring && self.gate.is_retiring(root),
            "recipient family is not retiring"
        );

        // This reservation excludes recovery and new turn starts while
        // checking every known family member and recording the disposition.
        {
            let _checkpoint = self.gate.try_checkpoint_retirement(root).context(
                "recipient family has an operator hold, reconciliation, or admitted work",
            )?;
            let _update = self.delivery_updates.lock().await;
            let mut members = BTreeSet::from([root.to_string()]);
            for (id, member) in store
                .list::<TeamAgentRuntime>("member", "", u32::MAX)
                .await?
            {
                if member.root_thread_id == root.to_string() {
                    members.insert(id);
                }
            }
            if let Some(manager) = self.manager.upgrade() {
                for id in manager.list_thread_ids().await {
                    if self.gate.root(id) != Some(root) {
                        continue;
                    }
                    members.insert(id.to_string());
                    let thread = manager.get_thread(id).await?;
                    ensure!(
                        !thread.team_has_active_or_finishing_turn().await,
                        "recipient family still has an active or finishing turn"
                    );
                    ensure!(
                        thread.list_background_terminals().await.is_empty(),
                        "recipient family still has background terminals"
                    );
                }
            }
            ensure!(
                members.contains(&params.thread_id),
                "recipient is not in the retiring family"
            );

            // Core can drain before the host consumes its durable terminal
            // callback. Let that callback establish the original outcome first.
            ensure!(
                !store
                    .list::<HostEvent>("lifecycle_finished", "", u32::MAX)
                    .await?
                    .iter()
                    .any(|(_, event)| matches!(event, HostEvent::Finished { thread_id, .. } if members.contains(thread_id))),
                "Waiting for terminal delivery evidence to settle; retry after the host processes its saved terminal events."
            );

            for id in &members {
                ensure!(
                    self.continuation_turn_id(ThreadId::from_string(id)?)
                        .await?
                        .is_none(),
                    "recipient family has an interrupted continuation"
                );
                ensure!(
                    self.jobs(id)
                        .await?
                        .iter()
                        .all(|job| job.status == TeamJobStatus::Cancelled),
                    "recipient family still has tracked external jobs"
                );
            }
            let (delivery, receipt) = store
                .deliveries(&params.thread_id)
                .await?
                .into_iter()
                .find(|(delivery, _)| delivery.id == params.delivery_id)
                .context("delivery is not addressed to this thread")?;
            ensure!(
                delivery.message.recipient.as_ref().is_none_or(|address| {
                    address.thread_id == params.thread_id
                        && address.machine_id == retirement.address.machine_id
                        && address.server_id == retirement.address.server_id
                }),
                "delivery recipient address differs from the retiring family"
            );
            ensure!(
                matches!(
                    receipt.state,
                    TeamReceiptState::Accepted | TeamReceiptState::NeedsReconciliation
                ),
                "only an admitted delivery with an unresolved outcome may be abandoned"
            );
            if let Some(attempt) = store
                .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
                .await?
            {
                ensure!(
                    !attempt
                        .participation
                        .values()
                        .any(|outcome| { matches!(outcome, Some(AttemptOutcome::Completed)) })
                        && !attempt
                            .terminal_outcomes
                            .values()
                            .any(|outcome| matches!(outcome, AttemptOutcome::Completed)),
                    "delivery has completed execution evidence; reconcile its receipt instead"
                );
            }

            // Missing dispatch/terminal evidence remains uncertainty. An explicit
            // disposition resolves retirement without inventing execution proof.
            let response = store
                .abandon_delivery_once(
                    &TeamDeliveryAbandonment {
                        operation_id: params.operation_id,
                        delivery_id: params.delivery_id,
                        thread_id: params.thread_id,
                        reason: params.reason,
                    },
                    &operation_key,
                    &digest,
                )
                .await?;
            drop(_update);
            drop(_checkpoint);
            drop(model_update);
            self.advance_retirements().await?;
            self.notify();
            Ok(response)
        }
    }
}
