use std::time::Duration;

use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::turn_input::TurnStartOptions;
use codex_rollout_trace::AgentResultTracePayload;
use tracing::debug;
use tracing::warn;

use super::Session;
use super::TurnContext;
use crate::agent::AgentStatus;
use crate::agent::agent_status_from_event;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::tools::handlers::multi_agents_common::build_agent_resume_config;

const PARENT_COMPLETION_DELIVERY_ATTEMPTS: usize = 3;
const PARENT_COMPLETION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const COMPLETION_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParentCompletionDelivery {
    Delivered,
    NotPublished,
    PersistenceFailed(String),
}

pub(super) struct ParentCompletionOutcome {
    pub(super) status: AgentStatus,
}

impl Session {
    /// Forwards terminal turn events from spawned MultiAgentV2 children to their direct parent.
    pub(super) async fn maybe_notify_parent_of_terminal_turn(
        &self,
        turn_context: &TurnContext,
        msg: &EventMsg,
    ) -> Option<ParentCompletionOutcome> {
        if turn_context.multi_agent_version != MultiAgentVersion::V2 {
            return None;
        }

        if !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
            return None;
        }

        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &turn_context.session_source
        else {
            return None;
        };

        let status = match turn_context.terminal_error.lock().await.take() {
            Some(error) => AgentStatus::Errored(error.message),
            None => agent_status_from_event(msg)?,
        };
        if is_final(&status) {
            let mut last_persistence_error = None;
            for attempt in 1..=PARENT_COMPLETION_DELIVERY_ATTEMPTS {
                let delivery = tokio::time::timeout(
                    PARENT_COMPLETION_ATTEMPT_TIMEOUT,
                    self.forward_child_completion_to_parent(
                        turn_context,
                        *parent_thread_id,
                        child_agent_path,
                        status.clone(),
                    ),
                )
                .await
                .unwrap_or_else(|_| {
                    ParentCompletionDelivery::PersistenceFailed(format!(
                        "delivery attempt exceeded {}s",
                        PARENT_COMPLETION_ATTEMPT_TIMEOUT.as_secs()
                    ))
                });
                match delivery {
                    ParentCompletionDelivery::Delivered
                    | ParentCompletionDelivery::NotPublished => {
                        return Some(ParentCompletionOutcome { status });
                    }
                    ParentCompletionDelivery::PersistenceFailed(error) => {
                        warn!(
                            child_thread_id = %self.thread_id,
                            %parent_thread_id,
                            attempt,
                            max_attempts = PARENT_COMPLETION_DELIVERY_ATTEMPTS,
                            %error,
                            "failed to persist terminal child delivery"
                        );
                        last_persistence_error = Some(error);
                        if attempt < PARENT_COMPLETION_DELIVERY_ATTEMPTS {
                            tokio::time::sleep(Duration::from_millis(
                                /*millis*/ 50 * attempt as u64,
                            ))
                            .await;
                        }
                    }
                }
            }
            let error = last_persistence_error.unwrap_or_else(|| "unknown error".to_string());
            let delivery_error = format!(
                "child reached a terminal state, but its result could not be durably delivered to parent {parent_thread_id} after {PARENT_COMPLETION_DELIVERY_ATTEMPTS} attempts: {error}; inspect child rollout {} for the original terminal event",
                self.thread_id
            );
            return Some(ParentCompletionOutcome {
                status: AgentStatus::Errored(delivery_error),
            });
        }
        Some(ParentCompletionOutcome { status })
    }

    /// Sends the standard completion envelope from a spawned MultiAgentV2 child to its parent.
    async fn forward_child_completion_to_parent(
        &self,
        turn_context: &TurnContext,
        parent_thread_id: ThreadId,
        child_agent_path: &AgentPath,
        status: AgentStatus,
    ) -> ParentCompletionDelivery {
        match self
            .services
            .agent_control
            .is_open_thread_spawn_child(parent_thread_id, self.thread_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => return ParentCompletionDelivery::NotPublished,
            Err(err) => {
                return ParentCompletionDelivery::PersistenceFailed(format!(
                    "failed to verify child admission: {err}"
                ));
            }
        }
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return ParentCompletionDelivery::PersistenceFailed(
                "child agent path does not identify a direct parent".to_string(),
            );
        };

        let Some(message) = format_inter_agent_completion_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            &status,
        ) else {
            return ParentCompletionDelivery::PersistenceFailed(
                "terminal child status could not be formatted".to_string(),
            );
        };
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = self
            .services
            .rollout_thread_trace
            .is_enabled()
            .then(|| message.clone());
        let mut communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path.clone(),
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        communication.id = Some(ResponseItemId::with_suffix(
            "msg",
            format!("completion-{}-{}", self.thread_id, turn_context.sub_id),
        ));
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, self.thread_id);
        let resume_config = match build_agent_resume_config(turn_context) {
            Ok(config) => config,
            Err(err) => {
                debug!("failed to build resume config for parent thread {parent_thread_id}: {err}");
                return ParentCompletionDelivery::PersistenceFailed(format!(
                    "failed to build parent resume configuration: {err}"
                ));
            }
        };
        if let Err(err) = self
            .services
            .agent_control
            .ensure_v2_agent_loaded_for_terminal_delivery_boxed(
                resume_config,
                parent_thread_id,
                /*parent*/ None,
            )
            .await
        {
            debug!("failed to load parent thread {parent_thread_id}: {err}");
            return ParentCompletionDelivery::PersistenceFailed(err.to_string());
        }
        if let Err(err) = Box::pin(self.services.agent_control.send_inter_agent_communication(
            parent_thread_id,
            communication,
            context,
            TurnStartOptions::default(),
        ))
        .await
        {
            debug!("failed to notify parent thread {parent_thread_id}: {err}");
            return ParentCompletionDelivery::PersistenceFailed(err.to_string());
        }
        let activity = async {
            if matches!(status, AgentStatus::Completed(_))
                && let Some(parent_turn_id) = turn_context.turn_metadata_state.parent_turn_id()
            {
                let initiating_thread_id = match turn_context
                    .turn_metadata_state
                    .initiating_agent_path()
                {
                    Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                        self.services
                            .agent_control
                            .resolve_agent_reference(
                                self.thread_id,
                                &turn_context.session_source,
                                initiating_agent_path.as_str(),
                            )
                            .await
                            .inspect_err(|err| {
                                debug!(
                                    "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                                );
                            })
                            .ok()
                    }
                    _ => Some(parent_thread_id),
                };
                if let Some(initiating_thread_id) = initiating_thread_id
                    && let Err(err) = self
                        .services
                        .agent_control
                        .emit_sub_agent_activity(
                            initiating_thread_id,
                            parent_turn_id,
                            SubAgentActivityItem {
                                id: format!("subagent-completed-{}", turn_context.sub_id),
                                kind: SubAgentActivityKind::Completed,
                                agent_thread_id: self.thread_id,
                                agent_path: child_agent_path.clone(),
                            },
                        )
                        .await
                {
                    debug!(
                        "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                    );
                }
            }
        };
        if tokio::time::timeout(COMPLETION_ACTIVITY_TIMEOUT, activity)
            .await
            .is_err()
        {
            debug!("timed out emitting best-effort completed sub-agent activity");
        }
        if let Some(message) = trace_message {
            self.services
                .rollout_thread_trace
                .record_agent_result_interaction(
                    turn_context.sub_id.as_str(),
                    parent_thread_id,
                    &AgentResultTracePayload {
                        child_agent_path: child_agent_path.as_str(),
                        message: &message,
                        status: &status,
                    },
                );
        }
        ParentCompletionDelivery::Delivered
    }
}
