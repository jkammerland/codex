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

impl CodexThread {
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

    /// Capture the active turn's settings separately from next-turn settings.
    pub async fn team_active_turn_model(&self) -> Option<(String, String, Option<String>)> {
        let active = self.session.active_turn.lock().await;
        let turn = &active.as_ref()?.task.as_ref()?.turn_context;
        let (model, effort) = turn.capture_current_model_selection();
        Some((turn.sub_id.clone(), model, effort))
    }

    /// Wait for turn cancellation before the host enumerates surviving jobs.
    pub async fn stop_team_turn(&self) -> Result<()> {
        if !self
            .session
            .services
            .extensions
            .execution_control()
            .snapshot(self.session.thread_id)
            .is_some_and(|state| state.mode == ExecutionMode::Stopped)
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
            })
            .await?;
        result.await.unwrap_or(Err(CodexErr::InternalAgentDied))
    }
}
