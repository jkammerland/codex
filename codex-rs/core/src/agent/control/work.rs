//! Durable mail receipts and assignment observations for the attached local tree.

use super::LocalAgentControl;
use crate::PendingAgentMessage;
use crate::agent::api::AgentBarrierSummary;
use crate::agent::api::AgentWorkSummary;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::MessageDelivery;
use codex_protocol::ResponseItemId;
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
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::PoisonError;

/// The storage task owns publication and access until the operation finishes.
/// Caller cancellation cannot release either before a queued commit completes.
struct WorkMutation {
    // Fields drop in declaration order: release storage before runtime or attachments.
    _store_access: tokio::sync::OwnedRwLockWriteGuard<()>,
    control: LocalAgentControl,
    _retained_mail: Option<PendingAgentMessage>,
}

impl Drop for WorkMutation {
    fn drop(&mut self) {
        let _barriers = self
            .control
            .runtime
            .barriers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.control
            .runtime
            .work_updates
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

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

    pub(super) async fn mutate_work<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        self.mutate_work_retaining(/*retained_mail*/ None, operation)
            .await
    }

    async fn mutate_work_retaining<T: Send + 'static>(
        &self,
        retained_mail: Option<PendingAgentMessage>,
        operation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let mutation = WorkMutation {
            control: self.clone(),
            _store_access: Arc::clone(&self.runtime.work_store_access)
                .write_owned()
                .await,
            _retained_mail: retained_mail,
        };
        // SQLx can finish a queued COMMIT after its waiting future is dropped.
        // Detach only the caller's wait, keeping the operation and guard together.
        tokio::spawn(async move {
            let _mutation = mutation;
            operation.await
        })
        .await
        .map_err(|err| CodexErr::Fatal(format!("agent work mutation failed: {err}")))?
    }

    pub(super) async fn accept_mail(
        &self,
        agent: ThreadId,
        pending: PendingAgentMessage,
        context: &AgentCommunicationContext,
    ) -> Result<(String, PendingAgentMessage)> {
        let (mut communication, delivery) = pending.into_parts();
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
            let pending = self.publish_mail(communication, delivery, context.sender_thread_id)?;
            return Ok((uuid::Uuid::now_v7().to_string(), pending));
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
            payload_json: serde_json::to_string(&communication)
                .map_err(|err| CodexErr::Fatal(err.to_string()))?,
            wake,
            task_id: task_id.clone(),
        };
        let store = self.mail_store().await?;
        let task = task_id
            .filter(|_| assignment)
            .map(|task_id| AgentTaskRecord {
                task_id,
                root_id: self.session_id.into(),
                owner_id: context.sender_thread_id,
                target_id: agent,
                name: Some(communication.recipient.to_string()),
                status: AgentTaskStatus::Queued,
                turn_id: None,
                result_ref: None,
            });
        let pending = self.publish_mail(communication, delivery, context.sender_thread_id)?;
        let registered = pending.clone();
        let live_mail = Arc::clone(&self.runtime.live_mail);
        self.mutate_work_retaining(Some(pending.clone()), async move {
            let registration = live_mail.register(agent, &registered);
            let accepted = if let Some(task) = task {
                store.accept_task(task, envelope).await
            } else {
                store.accept(envelope).await
            };
            accepted.map_err(|err| CodexErr::Fatal(err.to_string()))?;
            if let Some(registration) = registration {
                registration.committed();
            }
            Ok(())
        })
        .await?;
        Ok((message_id, pending))
    }

    /// Capture hooks once, after choosing the protocol ID but before durable visibility.
    fn publish_mail(
        &self,
        mut communication: InterAgentCommunication,
        delivery: Option<MessageDelivery>,
        sender: ThreadId,
    ) -> Result<PendingAgentMessage> {
        let manager = self.runtime.upgrade()?;
        let observers = manager.extensions.message_lifecycle_contributors();
        let delivery = delivery.or_else(|| {
            (!observers.is_empty()).then(|| {
                let message_id = communication
                    .id
                    .take()
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| ResponseItemId::new("amsg"));
                communication.id = Some(message_id.clone());
                MessageDelivery::publish(sender, message_id, observers)
            })
        });
        Ok(PendingAgentMessage::new(communication, delivery))
    }

    /// Settle only assignments selected for this exact terminal turn. Failed
    /// attempts need no submission receipt; success requires accepted input.
    /// Late mail stays queued and interruptions retain resumable assignments.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The read lock protects only the bounded storage snapshot from unfinished mutations; it is released before settlement writes, delivery, or model execution."
    )]
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
        let snapshot = {
            // An interrupted caller may leave its selected-input mutation still
            // committing. Settlement must observe that association before it
            // decides which assignments belong to the terminal turn.
            let _snapshot = self.runtime.work_store_access.read().await;
            store
                .tasks(
                    self.session_id.into(),
                    AgentTaskFilter {
                        target_id: Some(outcome.thread_id),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?
        };
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
                let store = Arc::clone(&store);
                self.mutate_work(async move {
                    store
                        .put_task(task)
                        .await
                        .map_err(|err| CodexErr::Fatal(err.to_string()))
                })
                .await?;
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
            let communication =
                self.publish_mail(communication, /*delivery*/ None, outcome.thread_id)?;
            {
                let store = Arc::clone(&store);
                let registered = communication.clone();
                let live_mail = Arc::clone(&self.runtime.live_mail);
                self.mutate_work_retaining(Some(communication.clone()), async move {
                    let registration = live_mail.register(owner_id, &registered);
                    store
                        .settle_task(task, envelope)
                        .await
                        .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                    if let Some(registration) = registration {
                        registration.committed();
                    }
                    Ok(())
                })
                .await?;
            }
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

    pub(super) async fn pending_mail_data(
        &self,
        agent: ThreadId,
    ) -> Result<(
        Vec<codex_protocol::protocol::InterAgentCommunication>,
        Vec<String>,
    )> {
        let store = self.mail_store().await?;
        let terminal = store
            .tasks(
                self.session_id.into(),
                codex_thread_store::AgentTaskFilter {
                    target_id: Some(agent),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| CodexErr::Fatal(err.to_string()))?
            .tasks
            .into_iter()
            .filter(|task| task.status.is_terminal())
            .map(|task| task.task_id)
            .collect::<HashSet<_>>();
        let records = store
            .pending(self.session_id.into(), agent)
            .await
            .map_err(|err| CodexErr::Fatal(err.to_string()))?;
        let mut pending = Vec::new();
        let mut discarded = Vec::new();
        for record in records {
            let communication = serde_json::from_str::<
                codex_protocol::protocol::InterAgentCommunication,
            >(&record.envelope.payload_json)
            .map_err(|err| CodexErr::Fatal(err.to_string()))?;
            if communication.mail.as_ref().is_some_and(|metadata| {
                metadata.kind == codex_protocol::protocol::AgentMailKind::Assignment
                    && metadata
                        .task_id
                        .as_ref()
                        .is_some_and(|id| terminal.contains(id))
            }) {
                discarded.push(record.envelope.message_id);
            } else {
                pending.push(communication);
            }
        }
        Ok((pending, discarded))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The read lock keeps counts and revision coherent across bounded storage snapshot reads; no delivery or model execution runs while it is held."
    )]
    pub(super) async fn work_summary(&self, agent: ThreadId) -> Result<AgentWorkSummary> {
        let store = self.mail_store().await?;
        let _snapshot = self.runtime.work_store_access.read().await;
        // Barrier selection and revision share one synchronous boundary. A barrier
        // change during the following reads belongs to a later snapshot.
        let (barrier, revision) = {
            let barriers = self
                .runtime
                .barriers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                barriers.get(&agent).cloned(),
                *self.runtime.work_updates.borrow(),
            )
        };
        // A summary needs only raw durable data. Attaching or pruning live handles
        // here could run extension destructors while the snapshot lock is held.
        let pending_mail = self.pending_mail_data(agent).await?.0.len();
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
            revision,
            pending_mail,
            waiting_tasks,
        })
    }
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
