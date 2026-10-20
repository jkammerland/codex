use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_state::validate_agent_task_completion;
use codex_state::validate_agent_task_identity;
use codex_state::validate_agent_task_request;
use codex_state::validate_agent_task_update;
use tokio::sync::Mutex;

use crate::AgentMailAcceptance;
use crate::AgentMailEnvelope;
use crate::AgentMailRecord;
use crate::AgentMailboxStore;
use crate::AgentMailboxSummary;
use crate::AgentTaskFilter;
use crate::AgentTaskRecord;
use crate::AgentTaskSnapshot;
use crate::AgentTaskStatus;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;

/// Explicitly ephemeral storage for sessions that do not promise persistence.
#[derive(Clone, Default)]
pub struct InMemoryAgentMailboxStore {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    sequence: i64,
    revisions: HashMap<ThreadId, i64>,
    mail: BTreeMap<String, AgentMailRecord>,
    tasks: BTreeMap<String, AgentTaskRecord>,
}

impl MemoryState {
    fn accept(&mut self, envelope: AgentMailEnvelope) -> ThreadStoreResult<AgentMailAcceptance> {
        if let Some(record) = self.mail.get(&envelope.message_id) {
            if record.envelope != envelope {
                return Err(invalid(
                    "message ID already accepted with a different envelope",
                ));
            }
            return Ok(AgentMailAcceptance {
                record: record.clone(),
                inserted: false,
            });
        }
        self.sequence += 1;
        *self.revisions.entry(envelope.root_id).or_default() += 1;
        let record = AgentMailRecord {
            envelope,
            sequence: self.sequence,
            recorded_turn_id: None,
            submitted_turn_id: None,
        };
        self.mail
            .insert(record.envelope.message_id.clone(), record.clone());
        Ok(AgentMailAcceptance {
            record,
            inserted: true,
        })
    }

    fn put_task(&mut self, task: AgentTaskRecord) -> ThreadStoreResult<()> {
        if let Some(previous) = self.tasks.get(&task.task_id) {
            validate_agent_task_update(previous, &task).map_err(invalid)?;
            if previous == &task {
                return Ok(());
            }
        }
        *self.revisions.entry(task.root_id).or_default() += 1;
        self.tasks.insert(task.task_id.clone(), task);
        Ok(())
    }

    fn mark_receipts(
        &mut self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
        receipt: Receipt,
    ) -> ThreadStoreResult<()> {
        // Validate the complete batch before mutating any receipt.
        for message_id in &message_ids {
            let record = self
                .mail
                .get(message_id)
                .filter(|record| {
                    record.envelope.root_id == root_id
                        && record.envelope.recipient_id == recipient_id
                })
                .ok_or_else(|| invalid("receipt references mail outside this recipient mailbox"))?;
            if matches!(receipt, Receipt::Submitted) && record.recorded_turn_id.is_none() {
                return Err(invalid("mail must be recorded before submission"));
            }
        }
        for message_id in message_ids {
            let record = self
                .mail
                .get_mut(&message_id)
                .ok_or_else(|| invalid("validated receipt mail is missing"))?;
            let slot = match receipt {
                Receipt::Attempted => None,
                Receipt::Recorded => Some(&mut record.recorded_turn_id),
                Receipt::Submitted => Some(&mut record.submitted_turn_id),
            };
            if let Some(slot) = slot
                && slot.is_none()
            {
                *slot = Some(turn_id.clone());
                *self.revisions.entry(root_id).or_default() += 1;
            }

            // A receipt is immutable, but held work can execute in a later turn.
            if !matches!(receipt, Receipt::Recorded)
                && let Some(task_id) = &record.envelope.task_id
                && let Some(task) = self.tasks.get_mut(task_id)
                && task.root_id == root_id
                && task.target_id == recipient_id
                && !task.status.is_terminal()
            {
                let status = match receipt {
                    Receipt::Submitted => AgentTaskStatus::Running,
                    Receipt::Attempted | Receipt::Recorded
                        if task.turn_id.as_ref() == Some(&turn_id)
                            && task.status == AgentTaskStatus::Running =>
                    {
                        AgentTaskStatus::Running
                    }
                    Receipt::Attempted | Receipt::Recorded => AgentTaskStatus::Queued,
                };
                if task.status != status || task.turn_id.as_ref() != Some(&turn_id) {
                    task.status = status;
                    task.turn_id = Some(turn_id.clone());
                    *self.revisions.entry(root_id).or_default() += 1;
                }
            }
        }
        Ok(())
    }
}

impl AgentMailboxStore for InMemoryAgentMailboxStore {
    fn accept(&self, envelope: AgentMailEnvelope) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move { self.state.lock().await.accept(envelope) })
    }

    fn pending(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<AgentMailRecord>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let mut records = state
                .mail
                .values()
                .filter(|record| {
                    record.envelope.root_id == root_id
                        && record.envelope.recipient_id == recipient_id
                        && record.submitted_turn_id.is_none()
                })
                .cloned()
                .collect::<Vec<_>>();
            records.sort_by_key(|record| record.sequence);
            Ok(records)
        })
    }

    fn accepted_ids(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
    ) -> ThreadStoreFuture<'_, Vec<String>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let ids = message_ids.into_iter().collect::<HashSet<_>>();
            let mut records = state
                .mail
                .values()
                .filter(|record| {
                    record.envelope.root_id == root_id
                        && record.envelope.recipient_id == recipient_id
                        && ids.contains(&record.envelope.message_id)
                })
                .collect::<Vec<_>>();
            records.sort_by_key(|record| record.sequence);
            Ok(records
                .into_iter()
                .map(|record| record.envelope.message_id.clone())
                .collect())
        })
    }

    fn mark_attempted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.mark_receipts(
                root_id,
                recipient_id,
                message_ids,
                turn_id,
                Receipt::Attempted,
            )
        })
    }

    fn mark_recorded(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.mark_receipts(
                root_id,
                recipient_id,
                message_ids,
                turn_id,
                Receipt::Recorded,
            )
        })
    }

    fn mark_submitted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.mark_receipts(
                root_id,
                recipient_id,
                message_ids,
                turn_id,
                Receipt::Submitted,
            )
        })
    }

    fn put_task(&self, task: AgentTaskRecord) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { self.state.lock().await.put_task(task) })
    }

    fn accept_task(
        &self,
        task: AgentTaskRecord,
        request: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move {
            validate_agent_task_request(&task, &request).map_err(invalid)?;
            let mut state = self.state.lock().await;
            if state.mail.contains_key(&request.message_id) {
                let previous = state
                    .tasks
                    .get(&task.task_id)
                    .ok_or_else(|| invalid("accepted task request has no task record"))?;
                validate_agent_task_identity(previous, &task).map_err(invalid)?;
                return state.accept(request);
            }
            if state.tasks.contains_key(&task.task_id) {
                return Err(invalid("task already has an accepted initial request"));
            }
            state.put_task(task)?;
            state.accept(request)
        })
    }

    fn settle_task(
        &self,
        task: AgentTaskRecord,
        completion: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move {
            validate_agent_task_completion(&task, &completion).map_err(invalid)?;
            let mut state = self.state.lock().await;
            // Check both identities before the atomic pair of in-memory writes.
            if let Some(previous) = state.tasks.get(&task.task_id) {
                validate_agent_task_update(previous, &task).map_err(invalid)?;
            }
            if let Some(previous) = state.mail.get(&completion.message_id)
                && previous.envelope != completion
            {
                return Err(invalid(
                    "message ID already accepted with a different envelope",
                ));
            }
            state.put_task(task)?;
            state.accept(completion)
        })
    }

    fn tasks(
        &self,
        root_id: ThreadId,
        filter: AgentTaskFilter,
    ) -> ThreadStoreFuture<'_, AgentTaskSnapshot> {
        Box::pin(async move {
            let state = self.state.lock().await;
            Ok(AgentTaskSnapshot {
                revision: state.revisions.get(&root_id).copied().unwrap_or_default(),
                tasks: state
                    .tasks
                    .values()
                    .filter(|task| task.root_id == root_id && filter.matches(task))
                    .cloned()
                    .collect(),
            })
        })
    }

    fn summary(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, AgentMailboxSummary> {
        Box::pin(async move {
            let state = self.state.lock().await;
            Ok(AgentMailboxSummary {
                revision: state.revisions.get(&root_id).copied().unwrap_or_default(),
                pending_mail: state
                    .mail
                    .values()
                    .filter(|record| {
                        record.envelope.root_id == root_id && record.submitted_turn_id.is_none()
                    })
                    .count() as i64,
                unsettled_tasks: state
                    .tasks
                    .values()
                    .filter(|task| task.root_id == root_id && !task.status.is_terminal())
                    .count() as i64,
            })
        })
    }

    fn remove_root(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state
                .mail
                .retain(|_, record| record.envelope.root_id != root_id);
            state.tasks.retain(|_, task| task.root_id != root_id);
            state.revisions.remove(&root_id);
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum Receipt {
    Attempted,
    Recorded,
    Submitted,
}

fn invalid(message: impl ToString) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: message.to_string(),
    }
}

#[cfg(test)]
#[path = "agent_mailbox_memory_tests.rs"]
mod tests;
