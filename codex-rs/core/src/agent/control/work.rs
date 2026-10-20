//! Durable mail receipts and assignment observations for the attached local tree.

use super::LocalAgentControl;
use crate::agent::api::AgentBarrierSummary;
use crate::agent::api::AgentWorkSummary;
use crate::agent_communication::AgentCommunicationContext;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentMailKind;
use codex_protocol::protocol::AgentMailMetadata;
use codex_protocol::protocol::AgentMailWake;
use codex_protocol::protocol::InterAgentCommunication;
use codex_thread_store::AgentMailEnvelope;
use codex_thread_store::AgentMailboxStore;
use codex_thread_store::AgentTaskFilter;
use codex_thread_store::AgentTaskRecord;
use codex_thread_store::AgentTaskStatus;
use std::sync::Arc;
use std::sync::PoisonError;

impl LocalAgentControl {
    pub(super) async fn mail_store(&self) -> Result<Arc<dyn AgentMailboxStore>> {
        let manager = self.runtime.upgrade()?;
        let store = self
            .runtime
            .mail_store
            .get_or_try_init(|| async {
                let root = manager.get_thread(self.session_id.into()).await?;
                if root.config().await.ephemeral {
                    Ok(
                        Arc::new(codex_thread_store::InMemoryAgentMailboxStore::default())
                            as Arc<dyn AgentMailboxStore>,
                    )
                } else {
                    manager.agent_mailbox_store().await
                }
            })
            .await?;
        Ok(Arc::clone(store))
    }

    pub(super) fn work_changed(&self) {
        self.runtime
            .work_updates
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(super) async fn accept_mail(
        &self,
        agent: ThreadId,
        communication: &mut InterAgentCommunication,
        context: &AgentCommunicationContext,
    ) -> Result<String> {
        let manager = self.runtime.upgrade()?;
        let feature_enabled = match manager.get_thread(self.session_id.into()).await {
            Ok(root) => root
                .session
                .enabled(codex_features::Feature::WakeOnAgentMessages),
            Err(_) => manager.get_thread(agent).await.is_ok_and(|thread| {
                thread
                    .session
                    .enabled(codex_features::Feature::WakeOnAgentMessages)
            }),
        };
        if !feature_enabled {
            communication.mail = None;
            return Ok(uuid::Uuid::now_v7().to_string());
        }
        let metadata = communication.mail.get_or_insert_with(|| AgentMailMetadata {
            message_id: codex_protocol::ResponseItemId::new("agentmail").to_string(),
            task_id: communication
                .trigger_turn
                .then(|| uuid::Uuid::now_v7().to_string()),
            kind: if communication.trigger_turn {
                AgentMailKind::Assignment
            } else {
                AgentMailKind::Completion
            },
            wake: AgentMailWake::Auto,
        });
        communication.id = Some(codex_protocol::ResponseItemId::from_server(
            metadata.message_id.clone(),
        ));
        let wake = metadata.wake == AgentMailWake::Auto;
        let assignment = metadata.kind == AgentMailKind::Assignment;
        let kind = serde_json::to_string(&metadata.kind)
            .map_err(|err| CodexErr::Fatal(err.to_string()))?;
        let message_id = metadata.message_id.clone();
        let task_id = metadata.task_id.clone();
        let envelope = AgentMailEnvelope {
            message_id: message_id.clone(),
            root_id: self.session_id.into(),
            sender_id: context.sender_thread_id,
            recipient_id: agent,
            kind,
            payload_json: serde_json::to_string(communication)
                .map_err(|err| CodexErr::Fatal(err.to_string()))?,
            wake,
            task_id: task_id.clone(),
        };
        let store = self.mail_store().await?;
        if let Some(task_id) = task_id.filter(|_| assignment) {
            store
                .accept_task(
                    AgentTaskRecord {
                        task_id,
                        root_id: self.session_id.into(),
                        owner_id: context.sender_thread_id,
                        target_id: agent,
                        name: Some(communication.recipient.to_string()),
                        status: AgentTaskStatus::Queued,
                        turn_id: None,
                        result_ref: None,
                    },
                    envelope,
                )
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?;
        } else {
            store
                .accept(envelope)
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?;
        }
        self.work_changed();
        Ok(message_id)
    }

    /// Settle only assignments selected for this exact terminal turn. Failed
    /// attempts need no submission receipt; success requires accepted input.
    /// Late mail stays queued and interruptions retain resumable assignments.
    pub(super) async fn settle_consumed_tasks(
        &self,
        outcome: &crate::agent::api::AgentTurnOutcome,
    ) -> Result<bool> {
        let manager = self.runtime.upgrade()?;
        if !manager
            .get_thread(outcome.thread_id)
            .await
            .is_ok_and(|thread| {
                thread
                    .session
                    .enabled(codex_features::Feature::WakeOnAgentMessages)
            })
        {
            return Ok(false);
        }
        let store = self.mail_store().await?;
        let snapshot = store
            .tasks(
                self.session_id.into(),
                AgentTaskFilter {
                    target_id: Some(outcome.thread_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| CodexErr::Fatal(err.to_string()))?;
        let mut settled_parent = false;
        for mut task in snapshot.tasks.into_iter().filter(|task| {
            !task.status.is_terminal() && task.turn_id.as_deref() == Some(outcome.turn_id.as_str())
        }) {
            task.status = match &outcome.status {
                codex_protocol::protocol::AgentStatus::Completed(_)
                    if task.status != AgentTaskStatus::Running =>
                {
                    continue;
                }
                codex_protocol::protocol::AgentStatus::Completed(_) => AgentTaskStatus::Completed,
                codex_protocol::protocol::AgentStatus::Errored(_)
                    if outcome.error_info
                        == Some(codex_protocol::protocol::CodexErrorInfo::TooManyDenials) =>
                {
                    AgentTaskStatus::Held
                }
                codex_protocol::protocol::AgentStatus::Errored(_) => AgentTaskStatus::Failed,
                codex_protocol::protocol::AgentStatus::Interrupted => AgentTaskStatus::Held,
                codex_protocol::protocol::AgentStatus::Shutdown => AgentTaskStatus::Held,
                codex_protocol::protocol::AgentStatus::NotFound => AgentTaskStatus::Unavailable,
                codex_protocol::protocol::AgentStatus::PendingInit
                | codex_protocol::protocol::AgentStatus::Running => continue,
            };
            if !task.status.is_terminal() {
                store
                    .put_task(task)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                self.work_changed();
                continue;
            }
            let author = outcome
                .source
                .get_agent_path()
                .unwrap_or_else(codex_protocol::AgentPath::root);
            let recipient = self
                .runtime
                .registry
                .agent_metadata_for_thread(task.owner_id)
                .and_then(|metadata| metadata.agent_path)
                .unwrap_or_else(codex_protocol::AgentPath::root);
            let Some(content) = crate::session_prefix::format_inter_agent_completion_message(
                recipient.clone(),
                author.clone(),
                &outcome.status,
            ) else {
                continue;
            };
            let message_id = codex_protocol::ResponseItemId::new("agentmail");
            let mut communication = InterAgentCommunication::new(
                author,
                recipient,
                Vec::new(),
                content,
                /*trigger_turn*/ false,
            );
            communication.id = Some(message_id.clone());
            communication.mail = Some(AgentMailMetadata {
                message_id: message_id.to_string(),
                task_id: Some(task.task_id.clone()),
                kind: AgentMailKind::Completion,
                wake: AgentMailWake::Auto,
            });
            task.result_ref = Some(message_id.to_string());
            let owner_id = task.owner_id;
            let envelope = AgentMailEnvelope {
                message_id: message_id.to_string(),
                root_id: self.session_id.into(),
                sender_id: outcome.thread_id,
                recipient_id: owner_id,
                kind: serde_json::to_string(&AgentMailKind::Completion)
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?,
                payload_json: serde_json::to_string(&communication)
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?,
                wake: true,
                task_id: Some(task.task_id.clone()),
            };
            store
                .settle_task(task, envelope)
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?;
            self.work_changed();
            settled_parent |= matches!(&outcome.source, codex_protocol::protocol::SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { parent_thread_id, .. }) if *parent_thread_id == owner_id);
            // The durable result above is authoritative even if the chat route is gone.
            let context = AgentCommunicationContext::new(
                crate::agent_communication::AgentCommunicationKind::Result,
                outcome.thread_id,
            );
            if let Err(err) = self
                .send_inter_agent_communication(
                    owner_id,
                    communication,
                    context,
                    crate::TurnStartOptions::default(),
                )
                .await
            {
                tracing::warn!("durable task result chat delivery failed: {err}");
            }
        }
        Ok(settled_parent)
    }

    pub(super) async fn work_summary(&self, agent: ThreadId) -> Result<AgentWorkSummary> {
        let store = self.mail_store().await?;
        let pending_mail = crate::agent::api::AgentControl::pending_mail(self, agent)
            .await?
            .len();
        let barrier = self
            .runtime
            .barriers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&agent)
            .cloned();
        let waiting_tasks = if let Some(task_ids) = barrier {
            let total = task_ids.len();
            let snapshot = store
                .tasks(
                    self.session_id.into(),
                    AgentTaskFilter {
                        task_ids,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?;
            Some(AgentBarrierSummary {
                total,
                remaining: total.saturating_sub(
                    snapshot
                        .tasks
                        .iter()
                        .filter(|task| task.status.is_terminal())
                        .count(),
                ),
                held: snapshot
                    .tasks
                    .iter()
                    .filter(|task| task.status == AgentTaskStatus::Held)
                    .count(),
                unavailable: snapshot
                    .tasks
                    .iter()
                    .filter(|task| {
                        matches!(
                            task.status,
                            AgentTaskStatus::Unavailable | AgentTaskStatus::CapacityUnavailable
                        )
                    })
                    .count(),
            })
        } else {
            None
        };
        Ok(AgentWorkSummary {
            revision: *self.runtime.work_updates.borrow(),
            pending_mail,
            waiting_tasks,
        })
    }
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
