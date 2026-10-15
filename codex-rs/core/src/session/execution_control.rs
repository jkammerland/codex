//! Cooperative checkpoints; cancellation remains owned by the running operation.

use codex_extension_api::ExecutionPermit;

use super::session::Session;

impl Session {
    /// Validate new team work against the host's cached catalog. A custom
    /// provider may use uncatalogued model names; its existing policy remains
    /// authoritative when it does not advertise that model.
    pub(crate) async fn team_model_error(
        &self,
        settings: &crate::codex_thread::ThreadConfigSnapshot,
    ) -> Option<String> {
        self.services
            .extensions
            .execution_control()
            .root(self.thread_id)?;
        let config = self.get_config().await;
        let models = self
            .services
            .models_manager
            .list_models(
                codex_models_manager::manager::RefreshStrategy::Offline,
                config.http_client_factory(),
            )
            .await;
        let Some(model) = models.iter().find(|model| model.model == settings.model) else {
            return (settings.model_provider_id == codex_model_provider_info::OPENAI_PROVIDER_ID)
                .then(|| format!("Team work is held: model '{}' is unavailable on this host; select an available model.", settings.model));
        };
        if let Some(effort) = &settings.reasoning_effort
            && !model.supported_reasoning_efforts.is_empty()
            && !model
                .supported_reasoning_efforts
                .iter()
                .any(|option| &option.effort == effort)
        {
            return Some(format!(
                "Team work is held: effort '{effort}' is unavailable for model '{}'; select a supported effort.",
                settings.model
            ));
        }
        None
    }

    /// Persist delivery markers before any model request, including compaction.
    /// Recovery can only retry absent input if no model could have acted on it.
    pub(crate) async fn flush_team_input(&self) -> codex_protocol::error::Result<()> {
        if self
            .services
            .extensions
            .execution_control()
            .root(self.thread_id)
            .is_some()
        {
            self.flush_rollout().await.map_err(|error| {
                codex_protocol::error::CodexErr::Fatal(format!(
                    "failed to persist team input: {error}"
                ))
            })?;
        }
        Ok(())
    }

    pub(crate) async fn enter_execution(&self) -> ExecutionPermit {
        self.services
            .extensions
            .execution_control()
            .enter(self.thread_id)
            .await
    }

    pub(crate) fn try_enter_execution(&self) -> Option<ExecutionPermit> {
        self.services
            .extensions
            .execution_control()
            .try_enter(self.thread_id)
    }
}
