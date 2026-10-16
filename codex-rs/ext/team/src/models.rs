//! Host intent survives a queued settings acknowledgement or connection loss.

use crate::TeamHostService;
use anyhow::Result;
use anyhow::ensure;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::team_model::TeamModelPolicy;
use codex_protocol::team_model::TeamModelSelection;
use codex_protocol::team_model::TeamSelection;
use codex_protocol::team_model::TeamSelectionOrigin;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LocalModelIntent {
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    pub policy: TeamModelPolicy,
    pub superseded_by: Option<i64>,
}

impl TeamHostService {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes sparse durable model intents and supersession fences"
    )]
    pub(crate) async fn journal_model_selection(
        &self,
        thread_id: ThreadId,
        settings: &ThreadSettingsOverrides,
    ) -> Result<Option<i64>> {
        let _update = self.model_updates.lock().await;
        let Some(store) = self.store.get() else {
            ensure!(
                settings.team_selection.is_none(),
                "coordinator model selection requires enrollment"
            );
            return Ok(None);
        };
        let Some(root) = self.gate.root(thread_id) else {
            ensure!(
                settings.team_selection.is_none(),
                "coordinator model selection requires enrollment"
            );
            return Ok(None);
        };
        ensure!(
            !store
                .get::<bool>("leaving", &root.to_string())
                .await?
                .unwrap_or(false),
            "the enrolled family is leaving"
        );
        if settings.team_selection.is_none()
            && settings.model.is_none()
            && settings.effort.is_none()
            && settings.collaboration_mode.is_none()
        {
            return Ok(None);
        }

        let id = thread_id.to_string();
        let previous = store
            .get::<serde_json::Value>("model_selection_request", &id)
            .await?;
        let previous_local = store
            .get::<LocalModelIntent>("model_local_intent", &id)
            .await?;
        let fence = store.get::<i64>("model_selection_superseded", &id).await?;
        let mut request = serde_json::json!({"selection": settings.team_selection, "model": settings.model, "effort": settings.effort});
        if let Some(selection) = &settings.team_selection {
            ensure!(
                fence.is_none_or(|superseded| selection.revision > superseded),
                "model selection was superseded by a local edit"
            );
            if let Some(previous) = &previous {
                let revision = previous
                    .pointer("/selection/revision")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| anyhow::anyhow!("persisted model selection is invalid"))?;
                ensure!(
                    selection.revision >= revision,
                    "model selection revision is stale"
                );
                if selection.revision == revision {
                    ensure!(
                        ["selection", "model", "effort"]
                            .iter()
                            .all(|field| previous[*field] == request[*field]),
                        "model selection revision was reused with different input"
                    );
                    return Ok(None);
                }
            }
        }

        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("host is shutting down"))?;
        let snapshot = manager.get_thread(thread_id).await?.config_snapshot().await;
        let previous_desired = match previous
            .as_ref()
            .filter(|request| {
                request
                    .pointer("/selection/revision")
                    .and_then(serde_json::Value::as_i64)
                    .is_some_and(|revision| fence.is_none_or(|fence| revision > fence))
            })
            .and_then(|request| request.get("desired"))
        {
            Some(desired) => Some(serde_json::from_value::<LocalModelIntent>(desired.clone())?),
            None => None,
        };
        let mut intent = previous_local
            .as_ref()
            .filter(|intent| intent.superseded_by.is_none())
            .cloned()
            .or(previous_desired)
            .unwrap_or(LocalModelIntent {
                model: snapshot.model,
                effort: snapshot.reasoning_effort,
                policy: snapshot.team_model_policy,
                superseded_by: None,
            });
        // Legacy coordinator journals have no complete desired snapshot. Merge
        // their selected fields once before a newer sparse edit supersedes them.
        if let Some(previous) = &previous {
            let selection: TeamModelSelection =
                serde_json::from_value(previous["selection"].clone())?;
            if fence.is_none_or(|fence| selection.revision > fence)
                && intent
                    .policy
                    .selection
                    .as_ref()
                    .is_none_or(|applied| selection.revision > applied.revision)
            {
                if !matches!(selection.model, TeamSelection::Keep)
                    && let Some(model) = previous["model"].as_str()
                {
                    intent.model = model.to_owned();
                }
                if !matches!(selection.effort, TeamSelection::Keep) {
                    intent.effort = serde_json::from_value(previous["effort"].clone())?;
                }
                intent
                    .policy
                    .apply(&selection)
                    .map_err(anyhow::Error::msg)?;
            }
        }
        if let Some(mode) = &settings.collaboration_mode {
            intent.model = mode.model().to_owned();
            intent.effort = mode.reasoning_effort();
        } else {
            if let Some(model) = &settings.model {
                intent.model.clone_from(model);
            }
            if let Some(effort) = &settings.effort {
                intent.effort.clone_from(effort);
            }
        }
        if let Some(selection) = &settings.team_selection {
            intent.policy.apply(selection).map_err(anyhow::Error::msg)?;
            request["desired"] = serde_json::to_value(&intent)?;
            let mut writes = vec![("model_selection_request", id.as_str(), request)];
            if let Some(mut local) = previous_local {
                local.superseded_by = Some(selection.revision);
                writes.push((
                    "model_local_intent",
                    id.as_str(),
                    serde_json::to_value(local)?,
                ));
            }
            store.put_records(&writes).await?;
            self.notify();
            return Ok(None);
        }

        if settings.model.is_some() || settings.collaboration_mode.is_some() {
            intent.policy.model_origin = TeamSelectionOrigin::Explicit;
        }
        if let Some(effort) = &settings.effort {
            intent.policy.effort_origin = if effort.is_some() {
                TeamSelectionOrigin::Explicit
            } else {
                TeamSelectionOrigin::Default
            };
        } else if let Some(mode) = &settings.collaboration_mode {
            intent.policy.effort_origin = if mode.reasoning_effort().is_some() {
                TeamSelectionOrigin::Explicit
            } else {
                TeamSelectionOrigin::Default
            };
        }
        let revision = fence
            .into_iter()
            .chain(intent.policy.superseded_revision)
            .chain(
                intent
                    .policy
                    .selection
                    .as_ref()
                    .map(|selection| selection.revision),
            )
            .chain(
                previous
                    .as_ref()
                    .and_then(|request| request.pointer("/selection/revision"))
                    .and_then(serde_json::Value::as_i64),
            )
            .max();
        intent.policy.superseded_revision = revision;
        let mut writes = vec![(
            "model_local_intent",
            id.as_str(),
            serde_json::to_value(&intent)?,
        )];
        if let Some(revision) = revision {
            writes.push((
                "model_selection_superseded",
                id.as_str(),
                serde_json::to_value(revision)?,
            ));
        }
        store.put_records(&writes).await?;
        self.notify();
        Ok(revision)
    }

    /// Restore only idempotent settings. Model requests and external effects are
    /// never replayed by this journal reconciliation.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes journal recovery and checkpointing so later intents cannot be overwritten"
    )]
    pub(crate) async fn reconcile_model_selection(&self, thread_id: ThreadId) -> Result<()> {
        let _update = self.model_updates.lock().await;
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let Some(root) = self.gate.root(thread_id) else {
            return Ok(());
        };
        if store
            .get::<bool>("leaving", &root.to_string())
            .await?
            .unwrap_or(false)
        {
            return Ok(());
        }
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let thread = manager.get_thread(thread_id).await?;
        let id = thread_id.to_string();
        let result = async {
            let local = store
                .get::<LocalModelIntent>("model_local_intent", &id)
                .await?
                .filter(|intent| intent.superseded_by.is_none());
            let request = store
                .get::<serde_json::Value>("model_selection_request", &id)
                .await?;
            let fence = store.get::<i64>("model_selection_superseded", &id).await?;
            let requested = request.as_ref().filter(|request| {
                request
                    .pointer("/selection/revision")
                    .and_then(serde_json::Value::as_i64)
                    .is_some_and(|revision| fence.is_none_or(|fence| revision > fence))
            });
            let requested_desired = requested
                .and_then(|request| request.get("desired"))
                .map(|desired| serde_json::from_value::<LocalModelIntent>(desired.clone()))
                .transpose()?;
            let desired = local.or(requested_desired);
            if let Some(intent) = desired {
                let current = thread.config_snapshot().await;
                if current.model == intent.model
                    && current.reasoning_effort == intent.effort
                    && current.team_model_policy == intent.policy
                {
                    return Ok(());
                }
                thread
                    .restore_thread_settings(codex_core::CodexThreadSettingsOverrides {
                        model: Some(intent.model),
                        effort: Some(intent.effort),
                        restore_model_policy: Some(intent.policy),
                        ..Default::default()
                    })
                    .await?;
                thread.checkpoint_thread_settings().await?;
            } else if let Some(request) = requested {
                let selection: TeamModelSelection =
                    serde_json::from_value(request["selection"].clone())?;
                let model = request["model"].as_str().map(str::to_owned);
                let effort = (!matches!(selection.effort, TeamSelection::Keep))
                    .then(|| serde_json::from_value(request["effort"].clone()))
                    .transpose()?;
                // The Core input loop can already be waiting for this journal's
                // lock. Restore directly while retaining its atomic revision checks.
                thread
                    .restore_thread_settings(codex_core::CodexThreadSettingsOverrides {
                        model,
                        effort,
                        team_selection: Some(selection),
                        ..Default::default()
                    })
                    .await?;
                thread.checkpoint_thread_settings().await?;
            }
            anyhow::Ok(())
        }
        .await;
        match &result {
            Ok(()) => store.delete("model_recovery_error", &id).await?,
            Err(error) => {
                store
                    .put("model_recovery_error", &id, &error.to_string())
                    .await?
            }
        }
        if let Some(control) = store.control().await? {
            self.apply_gate(&control).await?;
        }
        self.notify();
        result
    }
}
