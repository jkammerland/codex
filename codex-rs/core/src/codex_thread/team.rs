//! Narrow host capabilities for coordinated threads. The input loop owns routing.

use crate::session::Submission;
use codex_extension_api::ExecutionMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TeamMessageInput;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use tokio::sync::oneshot;

use super::CodexThread;

/// Live turn settings and interaction waits captured together for the team host.
pub struct TeamActiveTurn {
    pub turn_id: String,
    pub model: String,
    pub effort: Option<String>,
    pub waiting_for_approval: bool,
    pub waiting_for_input: bool,
}

impl CodexThread {
    /// Inspect the thread's resolved local shell without detecting or launching it again.
    pub fn team_local_shell_name(&self) -> &'static str {
        self.session.user_shell().name()
    }

    /// Journal model intent with the host before acknowledging its queue admission.
    pub async fn prepare_team_model_selection(
        &self,
        settings: &codex_protocol::protocol::ThreadSettingsOverrides,
    ) -> Result<Option<i64>> {
        let mut superseded: Option<i64> = None;
        for contributor in self
            .session
            .services
            .extensions
            .thread_lifecycle_contributors()
        {
            let revision = contributor
                .on_model_selection_requested(self.session.thread_id(), settings)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            if let Some(revision) = revision {
                superseded = Some(superseded.map_or(revision, |previous| previous.max(revision)));
            }
        }
        Ok(superseded)
    }

    pub async fn configured_default_model(&self) -> String {
        let config = self.config().await;
        self.session
            .services
            .models_manager
            .get_default_model(
                &config.configured_model_default,
                /*allow_provider_model_fallback*/ false,
                codex_models_manager::manager::RefreshStrategy::Offline,
                config.http_client_factory(),
            )
            .await
    }

    /// Reason queued team work cannot start with the selected next-turn model.
    pub async fn team_model_error(&self) -> Option<String> {
        self.session
            .team_model_error(&self.config_snapshot().await)
            .await
    }

    /// Cancel through an immutable MCP binding with the operator-selected
    /// provenance. A changed plugin or registration must not receive job IDs.
    pub async fn cancel_team_mcp_job(
        &self,
        server: &str,
        tool: &str,
        plugin_id: Option<&str>,
        registration_fingerprint: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<codex_protocol::mcp::CallToolResult> {
        self.session.refresh_mcp_if_dirty().await;
        let binding = self
            .session
            .services
            .mcp_runtime
            .current_binding_for_call(server)
            .await
            .ok_or_else(|| anyhow::anyhow!("cancellation server is unavailable"))?;
        let call = binding
            .prepare_call(server, tool)
            .ok_or_else(|| anyhow::anyhow!("cancellation tool is unavailable"))?;
        let config = self.config().await;
        let context = codex_extension_api::McpToolContext::from_prepared_call(
            &call,
            config.mcp_servers.get().get(server),
        );
        anyhow::ensure!(
            context.registration_fingerprint() == Some(registration_fingerprint),
            "cancellation server configuration changed since this job was launched"
        );
        anyhow::ensure!(
            match (plugin_id, context.source()) {
                (None, codex_extension_api::McpToolSource::Config) => true,
                (Some(expected), codex_extension_api::McpToolSource::Plugin { id, .. }) =>
                    expected == id,
                _ => false,
            },
            "cancellation tool provenance no longer matches its adapter"
        );
        call.call(Some(arguments), /*meta*/ None, /*timeout*/ None)
            .await
    }

    pub async fn team_execution_history(
        &self,
    ) -> codex_thread_store::ThreadStoreResult<codex_thread_store::StoredExecutionHistory> {
        let thread = self
            .session
            .live_thread_for_persistence("team recovery")
            .map_err(|error| codex_thread_store::ThreadStoreError::Internal {
                message: error.to_string(),
            })?;
        thread.load_execution_history().await
    }
    pub async fn team_active_turn_id(&self) -> Option<String> {
        let active = self.session.active_turn.lock().await;
        active
            .as_ref()?
            .task
            .as_ref()
            .map(|task| task.turn_context.sub_id.clone())
    }

    /// Capture waits with their owning turn so refreshes do not retain stale status events.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the active task and its interaction waits must belong to the same snapshot"
    )]
    pub async fn team_active_turn(&self) -> Option<TeamActiveTurn> {
        let active = self.session.active_turn.lock().await;
        let active = active.as_ref()?;
        let turn = &active.task.as_ref()?.turn_context;
        let state = active.turn_state.lock().await;
        let (model, effort) = turn.capture_current_model_selection();
        Some(TeamActiveTurn {
            turn_id: turn.sub_id.clone(),
            model,
            effort,
            waiting_for_approval: state.has_pending_approval(),
            waiting_for_input: state.has_pending_user_input(),
        })
    }

    /// Wait for turn cancellation before the host enumerates surviving jobs.
    pub async fn stop_team_turn(&self) -> Result<()> {
        if !self
            .session
            .services
            .extensions
            .execution_control()
            .snapshot(self.session.thread_id)
            .is_some_and(|state| state.mode == ExecutionMode::Stopped || state.reconciling)
        {
            return Err(CodexErr::InvalidRequest(
                "team stop requires a closed execution gate".into(),
            ));
        }
        self.session
            .abort_all_tasks(TurnAbortReason::Interrupted)
            .await;
        Ok(())
    }

    pub async fn resume_team_mailbox(&self) {
        self.session.maybe_start_turn_for_pending_work().await;
    }

    /// The host journals `submission_id` before calling. That identity survives
    /// transport loss; peers cannot select thread settings or override approvals.
    pub async fn deliver_team_message(
        &self,
        message: TeamMessageInput,
        submission_id: String,
        mode: TurnInputMode,
    ) -> Result<TurnInputSubmission> {
        if self
            .session
            .services
            .extensions
            .execution_control()
            .root(self.session.thread_id)
            .is_none()
        {
            return Err(CodexErr::InvalidRequest(
                "thread is not enrolled in a team".into(),
            ));
        }
        if !matches!(mode, TurnInputMode::Steer { .. }) {
            self.ensure_execution_capacity_for_turn_start(
                self.session.services.agent_control.as_ref(),
            )
            .await?;
        }
        let (reply, result) = oneshot::channel();
        self.io
            .submit_with_id(Submission {
                id: submission_id,
                op: Op::TurnInput {
                    request: Box::new(TurnInputRequest::new(TurnInput::TeamMessage(message))),
                    mode,
                    reply,
                },
                trace: None,
                parent_turn_id: None,
                root_turn_id: None,
                turn_extension_init: None,
                residency_guard: None,
                message_delivery: None,
            })
            .await?;
        result.await.unwrap_or(Err(CodexErr::InternalAgentDied))
    }
}
