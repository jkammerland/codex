//! Preserve sparse selections until a thread can resolve host defaults at readiness.

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::team_model::TeamModelSelection;
use codex_protocol::team_model::TeamSelection;
use serde::Deserialize;
use serde::Serialize;

use super::LocalModelIntent;
use crate::TeamHostService;
use crate::lifecycle::HostEvent;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct DeferredModelIntent {
    pub(super) base: Option<LocalModelIntent>,
    pub(super) model: TeamSelection,
    pub(super) resolved_model: Option<String>,
    pub(super) effort: TeamSelection,
    pub(super) resolved_effort: Option<ReasoningEffort>,
}

impl DeferredModelIntent {
    pub(super) async fn resolve(
        self,
        thread: &CodexThread,
        selection: &TeamModelSelection,
    ) -> Result<LocalModelIntent> {
        let mut intent = match self.base {
            Some(base) => base,
            None => {
                let snapshot = thread.config_snapshot().await;
                LocalModelIntent {
                    model: snapshot.model,
                    effort: snapshot.reasoning_effort,
                    policy: snapshot.team_model_policy,
                    superseded_by: None,
                }
            }
        };
        let model = match &self.model {
            TeamSelection::Keep => None,
            TeamSelection::Default => Some(thread.configured_default_model().await),
            TeamSelection::Value { value } => Some(value.clone()),
            TeamSelection::Inherit => self.resolved_model,
        };
        if let Some(model) = model {
            intent.model = model;
        }
        if self.effort != TeamSelection::Keep {
            intent.effort = self.resolved_effort;
        }

        // Coalesced fields retain earlier pending ownership even when the
        // latest sparse revision keeps that field.
        if let Some(origin) = self.model.origin() {
            intent.policy.model_origin = origin;
        }
        if let Some(origin) = self.effort.origin() {
            intent.policy.effort_origin = origin;
        }
        intent.policy.apply(selection).map_err(anyhow::Error::msg)?;
        Ok(intent)
    }
}

impl TeamHostService {
    /// A resumed thread is registered before Ready restores its durable settings.
    /// Defer only that thread's team work until its requested revision is applied.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "observes complete model journals and their application before team dispatch"
    )]
    pub(crate) async fn model_selection_pending(&self, thread_id: ThreadId) -> Result<bool> {
        let _update = self.model_updates.lock().await;
        let Some(store) = self.store.get() else {
            return Ok(false);
        };
        let id = thread_id.to_string();
        let requested = store
            .get::<serde_json::Value>("model_selection_request", &id)
            .await?
            .and_then(|request| {
                request
                    .pointer("/selection/revision")
                    .and_then(serde_json::Value::as_i64)
            });
        let Some(requested) = requested else {
            return Ok(false);
        };
        if store
            .get::<i64>("model_selection_superseded", &id)
            .await?
            .is_some_and(|fence| fence >= requested)
        {
            return Ok(false);
        }
        let Some(manager) = self.manager.upgrade() else {
            return Ok(true);
        };
        let Ok(thread) = manager.get_thread(thread_id).await else {
            return Ok(true);
        };
        let policy = thread.config_snapshot().await.team_model_policy;
        Ok(policy
            .selection
            .is_none_or(|applied| applied.revision < requested))
    }

    /// Journal an enrolled, unloaded thread's team selection before resume can dispatch mail.
    /// Other settings continue through the ordinary loaded-thread request path.
    pub async fn defer_model_selection(&self, params: &ThreadSettingsUpdateParams) -> Result<bool> {
        let Some(selection) = &params.team_selection else {
            return Ok(false);
        };
        let thread_id = ThreadId::from_string(&params.thread_id)?;
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("host is shutting down"))?;
        if manager.get_thread(thread_id).await.is_ok() {
            return Ok(false);
        }
        ensure!(
            params
                == &ThreadSettingsUpdateParams {
                    thread_id: params.thread_id.clone(),
                    team_selection: Some(selection.clone()),
                    model: params.model.clone(),
                    effort: params.effort.clone(),
                    ..Default::default()
                },
            "unloaded team selections cannot be combined with other thread settings"
        );
        ensure!(
            !(selection.model == TeamSelection::Keep && params.model.is_some()
                || selection.effort == TeamSelection::Keep && params.effort.is_some()),
            "team selection conflicts with a legacy model or effort update"
        );
        let model = match &selection.model {
            TeamSelection::Value { value } => Some(value.clone()),
            TeamSelection::Keep | TeamSelection::Default => None,
            TeamSelection::Inherit => params.model.clone(),
        };
        let effort = match &selection.effort {
            TeamSelection::Value { value } => Some(Some(serde_json::from_value(
                serde_json::Value::String(value.clone()),
            )?)),
            TeamSelection::Default => Some(None),
            TeamSelection::Keep => None,
            TeamSelection::Inherit => Some(params.effort.clone()),
        };
        self.journal_model_selection(
            thread_id,
            &ThreadSettingsOverrides {
                team_selection: Some(selection.clone()),
                model,
                effort,
                ..Default::default()
            },
        )
        .await?;

        // A concurrent load may have passed readiness while the journal lock
        // was held. Reuse its settings reconciliation event in that case.
        let _ = self
            .events
            .send(HostEvent::SettingsChanged(thread_id.to_string()));
        Ok(true)
    }
}
