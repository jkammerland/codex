use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;

use crate::AgentMailAcceptance;
use crate::AgentMailEnvelope;
use crate::AgentMailRecord;
use crate::AgentMailboxSummary;
use crate::AgentTaskFilter;
use crate::AgentTaskRecord;
use crate::AgentTaskSnapshot;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;

/// Storage-neutral accepted agent mail, delivery receipts, and task settlement.
/// Implementations must preserve acceptance idempotency and atomic settlement,
/// and must keep recorded mail pending until the request that includes it is submitted.
/// Callers own observer notifications; storage never polls or starts model turns.
pub trait AgentMailboxStore: Send + Sync {
    fn accept(&self, envelope: AgentMailEnvelope) -> ThreadStoreFuture<'_, AgentMailAcceptance>;
    fn pending(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<AgentMailRecord>>;
    /// Return the accepted subset of model input IDs, including submitted mail,
    /// without duplicates and in acceptance order.
    fn accepted_ids(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
    ) -> ThreadStoreFuture<'_, Vec<String>>;
    /// Associate only selected assignment IDs with an attempted turn. This does
    /// not create a submitted receipt or remove mail from pending.
    fn mark_attempted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()>;
    fn mark_recorded(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()>;
    fn mark_submitted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, ()>;
    fn put_task(&self, task: AgentTaskRecord) -> ThreadStoreFuture<'_, ()>;
    fn accept_task(
        &self,
        task: AgentTaskRecord,
        request: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance>;
    fn settle_task(
        &self,
        task: AgentTaskRecord,
        completion: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance>;
    fn tasks(
        &self,
        root_id: ThreadId,
        filter: AgentTaskFilter,
    ) -> ThreadStoreFuture<'_, AgentTaskSnapshot>;
    fn summary(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, AgentMailboxSummary>;
    fn remove_root(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, ()>;
}

/// Local persistence in the existing queue database. Failures are returned to callers.
#[derive(Clone)]
pub struct LocalAgentMailboxStore {
    state_db: StateDbHandle,
}

impl LocalAgentMailboxStore {
    pub fn new(state_db: StateDbHandle) -> Self {
        Self { state_db }
    }
}

impl AgentMailboxStore for LocalAgentMailboxStore {
    fn accept(&self, envelope: AgentMailEnvelope) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .accept(envelope)
                .await
                .map_err(storage_error)
        })
    }

    fn pending(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<AgentMailRecord>> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .pending(root_id, recipient_id)
                .await
                .map_err(storage_error)
        })
    }

    fn accepted_ids(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
    ) -> ThreadStoreFuture<'_, Vec<String>> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .accepted_ids(root_id, recipient_id, message_ids)
                .await
                .map_err(storage_error)
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
            self.state_db
                .agent_mailbox()
                .mark_attempted(root_id, recipient_id, message_ids, turn_id)
                .await
                .map_err(storage_error)
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
            self.state_db
                .agent_mailbox()
                .mark_recorded(root_id, recipient_id, message_ids, turn_id)
                .await
                .map_err(storage_error)
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
            self.state_db
                .agent_mailbox()
                .mark_submitted(root_id, recipient_id, message_ids, turn_id)
                .await
                .map_err(storage_error)
        })
    }

    fn put_task(&self, task: AgentTaskRecord) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .put_task(task)
                .await
                .map_err(storage_error)
        })
    }

    fn accept_task(
        &self,
        task: AgentTaskRecord,
        request: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .accept_task(task, request)
                .await
                .map_err(storage_error)
        })
    }

    fn settle_task(
        &self,
        task: AgentTaskRecord,
        completion: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .settle_task(task, completion)
                .await
                .map_err(storage_error)
        })
    }

    fn tasks(
        &self,
        root_id: ThreadId,
        filter: AgentTaskFilter,
    ) -> ThreadStoreFuture<'_, AgentTaskSnapshot> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .tasks(root_id, filter)
                .await
                .map_err(storage_error)
        })
    }

    fn summary(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, AgentMailboxSummary> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .summary(root_id)
                .await
                .map_err(storage_error)
        })
    }

    fn remove_root(&self, root_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .agent_mailbox()
                .remove_root(root_id)
                .await
                .map_err(storage_error)
        })
    }
}

fn storage_error(
    error: impl std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
) -> ThreadStoreError {
    let error = error.into();
    if let Some(error) = error.downcast_ref::<std::io::Error>()
        && error.kind() == std::io::ErrorKind::InvalidInput
    {
        return ThreadStoreError::InvalidRequest {
            message: error.to_string(),
        };
    }
    ThreadStoreError::Internal {
        message: format!("agent mailbox storage failed: {error}"),
    }
}
