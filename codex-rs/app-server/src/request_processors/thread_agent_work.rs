//! Read authoritative agent-work state without changing thread residency or execution.

use codex_app_server_protocol::ThreadAgentBarrier;
use codex_app_server_protocol::ThreadAgentWork;
use codex_core::CodexThread;

pub(super) async fn snapshot(thread: &CodexThread) -> Option<ThreadAgentWork> {
    match thread.agent_work().await {
        Ok(Some(work)) => Some(ThreadAgentWork {
            revision: work.revision,
            pending_mail: work.pending_mail as u64,
            waiting_tasks: work.waiting_tasks.map(|waiting| ThreadAgentBarrier {
                total: waiting.total as u64,
                remaining: waiting.remaining as u64,
                held: waiting.held as u64,
                unavailable: waiting.unavailable as u64,
            }),
        }),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "native agent work snapshot unavailable");
            None
        }
    }
}

// A shared controller revision also changes for other agents. Publish only changes
// to this thread's visible state, without polling or repeating identical counts.
pub(super) fn same_state(left: &Option<ThreadAgentWork>, right: &Option<ThreadAgentWork>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.pending_mail == right.pending_mail && left.waiting_tasks == right.waiting_tasks
        }
        (None, None) => true,
        _ => false,
    }
}

/// Order a fresh work snapshot after publishing the initial thread identity.
pub(super) async fn publish(
    thread_id: codex_protocol::ThreadId,
    thread: &CodexThread,
    outgoing: &crate::outgoing_message::OutgoingMessageSender,
) {
    if let Some(agent_work) = snapshot(thread).await {
        outgoing
            .send_server_notification(
                codex_app_server_protocol::ServerNotification::ThreadAgentWorkUpdated(
                    codex_app_server_protocol::ThreadAgentWorkUpdatedNotification {
                        thread_id: thread_id.to_string(),
                        agent_work: Some(agent_work),
                    },
                ),
            )
            .await;
    }
}
