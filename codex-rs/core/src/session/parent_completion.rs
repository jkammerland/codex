use std::time::Duration;

use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::ErrorEvent;
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
use crate::session_prefix::bounded_completion_agent_path;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::tools::handlers::multi_agents_common::build_agent_resume_config;

const PARENT_COMPLETION_DELIVERY_ATTEMPTS: usize = 3;
const PARENT_COMPLETION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const COMPLETION_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParentCompletionDelivery {
    Delivered(ResponseItemId),
    NotPublished,
    PersistenceFailed(String),
}

impl Session {
    /// Forwards terminal turn events from spawned MultiAgentV2 children to their direct parent.
    pub(super) async fn maybe_notify_parent_of_terminal_turn(
        &self,
        turn_context: &TurnContext,
        msg: &EventMsg,
        status: AgentStatus,
    ) -> Result<(), String> {
        if turn_context.multi_agent_version != MultiAgentVersion::V2
            || !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
        {
            return Ok(());
        }
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &turn_context.session_source
        else {
            return Ok(());
        };
        if !is_final(&status) {
            return Ok(());
        }
        let completion_turn_id = terminal_event_turn_id(msg).unwrap_or(&turn_context.sub_id);
        self.deliver_and_record_parent_completion(
            turn_context,
            completion_turn_id,
            *parent_thread_id,
            child_agent_path,
            status,
        )
        .await
    }

    /// Replays a durable child outbox entry only when its deterministic acknowledgement is absent.
    pub(super) async fn resume_pending_parent_completion(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> Result<(), String> {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &turn_context.session_source
        else {
            return Ok(());
        };
        let Some((completion_turn_id, status)) = terminal_status_marker_from_rollout(rollout_items)
        else {
            return Ok(());
        };
        let delivery_id = parent_completion_delivery_id(self.thread_id, completion_turn_id);
        if rollout_items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::InterAgentCommunicationMetadata {
                    delivery_id: Some(recorded_id),
                    ..
                } if recorded_id == &delivery_id
            )
        }) {
            return Ok(());
        }
        self.deliver_and_record_parent_completion(
            turn_context,
            completion_turn_id,
            *parent_thread_id,
            child_agent_path,
            status.clone(),
        )
        .await
    }

    async fn deliver_and_record_parent_completion(
        &self,
        turn_context: &TurnContext,
        completion_turn_id: &str,
        parent_thread_id: ThreadId,
        child_agent_path: &AgentPath,
        status: AgentStatus,
    ) -> Result<(), String> {
        let delivery_id = {
            let mut handled_id = None;
            let mut last_error = None;
            for attempt in 1..=PARENT_COMPLETION_DELIVERY_ATTEMPTS {
                let delivery = tokio::time::timeout(
                    PARENT_COMPLETION_ATTEMPT_TIMEOUT,
                    self.forward_child_completion_to_parent(
                        turn_context,
                        completion_turn_id,
                        parent_thread_id,
                        child_agent_path,
                        status.clone(),
                    ),
                )
                .await;
                match delivery {
                    Err(_) => {
                        let error = format!(
                            "parent completion delivery attempt timed out after {PARENT_COMPLETION_ATTEMPT_TIMEOUT:?}"
                        );
                        last_error = Some(error.clone());
                        warn!(
                            child_thread_id = %self.thread_id,
                            %parent_thread_id,
                            attempt,
                            max_attempts = PARENT_COMPLETION_DELIVERY_ATTEMPTS,
                            %error,
                            "terminal child delivery remains pending"
                        );
                    }
                    Ok(ParentCompletionDelivery::Delivered(delivery_id)) => {
                        handled_id = Some(delivery_id);
                        break;
                    }
                    Ok(ParentCompletionDelivery::NotPublished) => {
                        handled_id = Some(parent_completion_delivery_id(
                            self.thread_id,
                            completion_turn_id,
                        ));
                        break;
                    }
                    Ok(ParentCompletionDelivery::PersistenceFailed(error)) => {
                        last_error = Some(error.clone());
                        warn!(
                            child_thread_id = %self.thread_id,
                            %parent_thread_id,
                            attempt,
                            max_attempts = PARENT_COMPLETION_DELIVERY_ATTEMPTS,
                            %error,
                            "terminal child delivery remains pending"
                        );
                    }
                }
            }
            handled_id.ok_or_else(|| {
                last_error.unwrap_or_else(|| "parent completion delivery failed".to_string())
            })?
        };
        let mut last_error = None;
        for attempt in 1..=PARENT_COMPLETION_DELIVERY_ATTEMPTS {
            let acknowledgement = tokio::time::timeout(
                PARENT_COMPLETION_ATTEMPT_TIMEOUT,
                self.record_parent_completion_delivery(delivery_id.clone()),
            )
            .await;
            match acknowledgement {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => {
                    last_error = Some(format!(
                        "parent completion acknowledgement timed out after {PARENT_COMPLETION_ATTEMPT_TIMEOUT:?}"
                    ));
                }
            }
            warn!(
                child_thread_id = %self.thread_id,
                %parent_thread_id,
                attempt,
                max_attempts = PARENT_COMPLETION_DELIVERY_ATTEMPTS,
                error = %last_error.as_deref().unwrap_or("unknown acknowledgement failure"),
                "terminal child delivery acknowledgement remains pending"
            );
        }
        Err(last_error.unwrap_or_else(|| "parent completion acknowledgement failed".to_string()))
    }

    async fn record_parent_completion_delivery(
        &self,
        delivery_id: ResponseItemId,
    ) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items(&[RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                    mail_fingerprint: None,
                    delivery_id: Some(delivery_id),
                }])
                .await
                .map_err(std::io::Error::other)?;
        }
        self.flush_rollout().await
    }

    /// Sends the standard completion envelope from a spawned MultiAgentV2 child to its parent.
    async fn forward_child_completion_to_parent(
        &self,
        turn_context: &TurnContext,
        completion_turn_id: &str,
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
            bounded_completion_agent_path(&parent_agent_path),
            bounded_completion_agent_path(child_agent_path),
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
        let delivery_id = parent_completion_delivery_id(self.thread_id, completion_turn_id);
        communication.id = Some(delivery_id.clone());
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
                                id: format!("subagent-completed-{completion_turn_id}"),
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
                    completion_turn_id,
                    parent_thread_id,
                    &AgentResultTracePayload {
                        child_agent_path: child_agent_path.as_str(),
                        message: &message,
                        status: &status,
                    },
                );
        }
        ParentCompletionDelivery::Delivered(delivery_id)
    }
}

pub(super) fn terminal_status_from_event(
    terminal_event: &EventMsg,
    preceding_error: Option<&ErrorEvent>,
) -> Option<AgentStatus> {
    let status = agent_status_from_event(terminal_event)?;
    if is_final(&status) {
        Some(status)
    } else if let Some(error) = preceding_error.filter(|error| error.affects_turn_status()) {
        Some(AgentStatus::Errored(error.message.clone()))
    } else {
        Some(status)
    }
}

pub(super) fn terminal_status_from_rollout(rollout_items: &[RolloutItem]) -> Option<AgentStatus> {
    if let Some((_, status)) = terminal_status_marker_from_rollout(rollout_items) {
        return Some(status.clone());
    }
    let terminal_index = rollout_items.iter().rposition(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
        )
    })?;
    if rollout_items[terminal_index + 1..].iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnStarted(_) | EventMsg::ThreadRolledBack(_))
        )
    }) {
        return None;
    }
    let RolloutItem::EventMsg(terminal_event) = &rollout_items[terminal_index] else {
        return None;
    };
    let preceding_error = rollout_items[..terminal_index]
        .iter()
        .rev()
        .take_while(|item| {
            !matches!(
                item,
                RolloutItem::EventMsg(
                    EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
                )
            )
        })
        .find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::Error(error)) => Some(error),
            _ => None,
        });
    terminal_status_from_event(terminal_event, preceding_error)
}

fn terminal_status_marker_from_rollout(
    rollout_items: &[RolloutItem],
) -> Option<(&str, &AgentStatus)> {
    for item in rollout_items.iter().rev() {
        match item {
            RolloutItem::TerminalAgentStatus { turn_id, status } => {
                return Some((turn_id, status));
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(_) | EventMsg::ThreadRolledBack(_)) => {
                return None;
            }
            _ => {}
        }
    }
    None
}

pub(super) fn terminal_event_turn_id(event: &EventMsg) -> Option<&str> {
    match event {
        EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
        EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
        _ => None,
    }
}

fn parent_completion_delivery_id(thread_id: ThreadId, turn_id: &str) -> ResponseItemId {
    ResponseItemId::with_suffix("msg", format!("completion-{thread_id}-{turn_id}"))
}

#[cfg(test)]
#[path = "parent_completion_tests.rs"]
mod tests;
