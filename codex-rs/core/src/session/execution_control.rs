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

    /// Persist the selected team input before any model request, including
    /// compaction. The callback records request participation, not just history.
    pub(crate) async fn flush_team_input(
        &self,
        turn_id: &str,
        turn_store: &codex_extension_api::ExtensionData,
        input: &[codex_protocol::models::ResponseItem],
    ) -> codex_protocol::error::Result<()> {
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
            for contributor in self.services.extensions.turn_lifecycle_contributors() {
                contributor
                    .on_model_request(codex_extension_api::TurnModelRequestInput {
                        turn_id,
                        input,
                        thread_store: &self.services.thread_extension_data,
                        turn_store,
                    })
                    .await
                    .map_err(codex_protocol::error::CodexErr::Fatal)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn acknowledge_team_request(
        &self,
        turn_store: &codex_extension_api::ExtensionData,
        response_id: Option<&str>,
    ) -> codex_protocol::error::Result<()> {
        if self
            .services
            .extensions
            .execution_control()
            .root(self.thread_id)
            .is_some()
        {
            for contributor in self.services.extensions.turn_lifecycle_contributors() {
                contributor
                    .on_model_acknowledged(codex_extension_api::TurnModelAcknowledgedInput {
                        response_id,
                        thread_store: &self.services.thread_extension_data,
                        turn_store,
                    })
                    .await
                    .map_err(codex_protocol::error::CodexErr::Fatal)?;
            }
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

    pub(crate) fn try_enter_turn_start(&self) -> Option<ExecutionPermit> {
        self.services
            .extensions
            .execution_control()
            .try_start_turn(self.thread_id)
    }

    /// Auxiliary input-loop operations report a held start through their normal event path.
    pub(crate) async fn try_enter_turn_start_or_report(
        &self,
        submission_id: &str,
    ) -> Option<ExecutionPermit> {
        let permit = self.try_enter_turn_start();
        if permit.is_none() {
            let error: codex_protocol::error::CodexErr =
                codex_protocol::error::CodexErrorDetails::TeamExecutionHeld.into();
            self.send_event_raw(codex_protocol::protocol::Event {
                id: submission_id.to_owned(),
                msg: codex_protocol::protocol::EventMsg::Error(
                    error.to_error_event(/*message_prefix*/ None),
                ),
            })
            .await;
        }
        permit
    }
}
