//! Retirement drains admitted work without using the operator Stop control.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;

use crate::TeamHostService;

impl TeamHostService {
    pub(crate) async fn retirement_states(&self) -> Result<Vec<TeamMemberRetirement>> {
        let Some(store) = self.store.get() else {
            return Ok(Vec::new());
        };
        Ok(store
            .list("retirement", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, state)| state)
            .collect())
    }

    pub(crate) async fn apply_retirement(&self, intent: &TeamMemberRetirement) -> Result<()> {
        let store = self.store.get().context("team host is not enrolled")?;
        let root = ThreadId::from_string(&intent.address.thread_id)?;
        if let Some(previous) = store
            .get::<TeamMemberRetirement>("retirement", &root.to_string())
            .await?
        {
            if previous.operation_id == intent.operation_id {
                return Ok(());
            }
            ensure!(
                previous.state == TeamMemberRetirementState::Removed,
                "a different retirement is already in progress"
            );
        }
        let enrollment = store
            .enrollment(&root.to_string())
            .await?
            .context("member to remove is not enrolled")?;
        ensure!(
            enrollment.address == intent.address,
            "retirement address differs from enrollment"
        );
        ensure!(
            intent.state == TeamMemberRetirementState::Retiring,
            "new retirement must be retiring"
        );
        // Persist before closing startup. Recovery installs this fence before
        // any enrolled session can be recovered or receive another turn.
        store.put("retirement", &root.to_string(), intent).await?;
        self.gate.begin_retirement(root);
        self.notify();
        Ok(())
    }

    pub(crate) async fn reject_retiring_deliveries(&self, thread_id: ThreadId) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        for (_, mut receipt) in store.deliveries(&thread_id.to_string()).await? {
            if receipt.state == TeamReceiptState::Pending {
                receipt.state = TeamReceiptState::Rejected;
                receipt.detail =
                    Some("Recipient is being removed; this input was not admitted.".into());
                store.update_receipt(&receipt).await?;
            }
        }
        Ok(())
    }

    /// The caller owns operations; settings/control mutations stay serialized
    /// while the final checkpoint is written and the boundary is detached.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "retirement checkpoints serialize with model selections and controls"
    )]
    pub(crate) async fn advance_retirements(&self) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let _model = self.model_updates.lock().await;
        for mut retirement in self.retirement_states().await? {
            if retirement.state == TeamMemberRetirementState::Removed {
                continue;
            }
            let root = ThreadId::from_string(&retirement.address.thread_id)?;
            let previous = retirement.clone();
            self.gate.begin_retirement(root);
            let mut members = std::collections::BTreeSet::from([root.to_string()]);
            for (id, member) in store
                .list::<TeamAgentRuntime>("member", "", u32::MAX)
                .await?
            {
                if member.root_thread_id == root.to_string() {
                    members.insert(id);
                }
            }
            let mut loaded = Vec::new();
            let mut waiting = self.gate.retirement_blocker(root).map(str::to_owned);
            if let Some(manager) = self.manager.upgrade() {
                for id in manager.list_thread_ids().await {
                    if self.gate.root(id) != Some(root) {
                        continue;
                    }
                    members.insert(id.to_string());
                    let thread = manager.get_thread(id).await?;
                    if thread.team_has_active_or_finishing_turn().await {
                        waiting.get_or_insert_with(|| {
                            "Waiting for the current turn to finish.".into()
                        });
                    }
                    if !thread.list_background_terminals().await.is_empty() {
                        waiting.get_or_insert_with(|| {
                            "Waiting for background terminals to finish.".into()
                        });
                    }
                    loaded.push((id.to_string(), thread));
                }
            }
            for id in &members {
                self.reject_retiring_deliveries(ThreadId::from_string(id)?)
                    .await?;
                if self
                    .continuation_turn_id(ThreadId::from_string(id)?)
                    .await?
                    .is_some()
                {
                    waiting.get_or_insert_with(|| {
                        "Waiting for the operator to resolve an interrupted turn.".into()
                    });
                }
                let unsettled_jobs = self
                    .jobs(id)
                    .await?
                    .into_iter()
                    .filter(|job| job.status != TeamJobStatus::Cancelled)
                    .map(|job| format!("{} ({:?})", job.id, job.status))
                    .collect::<Vec<_>>();
                if !unsettled_jobs.is_empty() {
                    waiting.get_or_insert_with(|| format!("Waiting for tracked external jobs: {}. Natural completion is not tracked by these adapters; the operator can resolve them through existing Stop/cancellation controls.", unsettled_jobs.join(", ")));
                }
                for (delivery, receipt) in store.deliveries(id).await? {
                    if matches!(
                        receipt.state,
                        TeamReceiptState::Accepted | TeamReceiptState::NeedsReconciliation
                    ) && store
                        .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                        .await?
                        .is_none()
                    {
                        waiting.get_or_insert_with(|| {
                            "Waiting for admitted delivery evidence to settle.".into()
                        });
                    }
                }
            }
            if waiting.is_none() {
                for id in &members {
                    let requested = store
                        .get::<serde_json::Value>("model_selection_request", id)
                        .await?
                        .and_then(|request| {
                            request
                                .pointer("/selection/revision")
                                .and_then(serde_json::Value::as_i64)
                        });
                    if let Some(revision) =
                        requested.max(store.get::<i64>("model_selection_superseded", id).await?)
                    {
                        store
                            .put("model_selection_superseded", id, &revision)
                            .await?;
                    }
                }
                for (id, thread) in &loaded {
                    let mut policy = thread.config_snapshot().await.team_model_policy;
                    let requested = store
                        .get::<serde_json::Value>("model_selection_request", id)
                        .await?
                        .and_then(|request| {
                            request
                                .pointer("/selection/revision")
                                .and_then(serde_json::Value::as_i64)
                        });
                    let revision = policy
                        .selection
                        .as_ref()
                        .map(|selection| selection.revision)
                        .max(policy.superseded_revision)
                        .max(requested)
                        .max(store.get::<i64>("model_selection_superseded", id).await?);
                    if let Some(revision) = revision {
                        store
                            .put("model_selection_superseded", id, &revision)
                            .await?;
                        policy.superseded_revision = Some(revision);
                        thread
                            .restore_thread_settings(codex_core::CodexThreadSettingsOverrides {
                                restore_model_policy: Some(policy),
                                ..Default::default()
                            })
                            .await?;
                        thread.checkpoint_thread_settings().await?;
                    }
                }
                // Checkpoints never own priority controls. Only final admission
                // and detachment share this short control mutation interval.
                if let Some(_checkpoint) = self.gate.try_checkpoint_retirement(root) {
                    // A recovery can load a previously unloaded family member
                    // while history checkpoints are being written.
                    if let Some(manager) = self.manager.upgrade() {
                        for id in manager.list_thread_ids().await {
                            if self.gate.root(id) != Some(root) {
                                continue;
                            }
                            if !loaded
                                .iter()
                                .any(|(loaded_id, _)| *loaded_id == id.to_string())
                            {
                                waiting = Some(
                                    "Waiting to checkpoint a newly loaded family member.".into(),
                                );
                                break;
                            }
                            let thread = manager.get_thread(id).await?;
                            if thread.team_has_active_or_finishing_turn().await
                                || !thread.list_background_terminals().await.is_empty()
                            {
                                waiting = Some("Waiting for recovered work to finish.".into());
                                break;
                            }
                        }
                    }
                    let _control = self.control_updates.lock().await;
                    if waiting.is_none() {
                        waiting = self.gate.retirement_blocker(root).map(str::to_owned);
                    }
                    if waiting.is_none() {
                        // Retirement survives credential revocation if a manual
                        // hold wins the final gate check or the host restarts.
                        store
                            .unenroll(
                                &root.to_string(),
                                &members.into_iter().collect::<Vec<_>>(),
                                None,
                            )
                            .await?;
                        if self.gate.complete_retirement(root) {
                            retirement.state = TeamMemberRetirementState::Removed;
                        } else {
                            waiting = Some(
                                self.gate
                                    .retirement_blocker(root)
                                    .unwrap_or("Waiting for admission to settle.")
                                    .into(),
                            );
                        }
                    }
                } else {
                    waiting = Some(
                        self.gate
                            .retirement_blocker(root)
                            .unwrap_or("Waiting for admission to settle.")
                            .into(),
                    );
                }
            }
            retirement.waiting_reason = waiting;
            if retirement != previous {
                store
                    .put("retirement", &root.to_string(), &retirement)
                    .await?;
                self.notify();
            }
        }
        Ok(())
    }
}
