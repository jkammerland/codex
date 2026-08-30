use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 400;
const COMPLETION_PAYLOAD_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const COMPLETION_AGENT_PATH_MAX_BYTES: usize = 64;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
) -> Option<String> {
    let task_name = bounded_completion_agent_path(&task_name);
    let sender = bounded_completion_agent_path(&sender);
    let payload = match status {
        AgentStatus::Completed(Some(message)) => truncate_text(
            message,
            TruncationPolicy::Tokens(COMPLETION_PAYLOAD_MAX_TOKENS),
        ),
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_text(
                error,
                TruncationPolicy::Tokens(COMPLETION_PAYLOAD_MAX_TOKENS),
            );
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(InterAgentCompletionMessage::new(task_name, sender, payload).render())
}

pub(crate) fn bounded_completion_agent_path(path: &AgentPath) -> AgentPath {
    if path.as_str().len() <= COMPLETION_AGENT_PATH_MAX_BYTES {
        return path.clone();
    }
    let digest = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, path.as_str().as_bytes());
    match AgentPath::root().join(&format!("truncated_{}", digest.simple())) {
        Ok(path) => path,
        Err(_) => AgentPath::root(),
    }
}

#[cfg(test)]
#[path = "session_prefix_tests.rs"]
mod tests;

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}
