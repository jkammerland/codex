use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_utils_output_truncation::approx_token_count;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::bounded_completion_agent_path;
use super::format_inter_agent_completion_message;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

#[test]
fn successful_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Completed(Some("finished work ".repeat(1_000))),
    )
    .expect("completed status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
}

#[test]
fn completion_envelope_with_long_agent_paths_is_bounded() {
    let long_name = "a".repeat(50_000);
    let task_name = AgentPath::try_from(format!("/root/{long_name}").as_str())
        .expect("long task path should be valid");
    let sender = AgentPath::try_from(format!("/root/{long_name}/worker").as_str())
        .expect("long sender path should be valid");
    let bounded_task_name = bounded_completion_agent_path(&task_name);
    let bounded_sender = bounded_completion_agent_path(&sender);
    let message = format_inter_agent_completion_message(
        bounded_task_name.clone(),
        bounded_sender.clone(),
        &AgentStatus::Completed(Some("done".to_string())),
    )
    .expect("completed status should produce a completion message");

    let communication = InterAgentCommunication::new(
        bounded_sender,
        bounded_task_name,
        Vec::new(),
        message,
        /*trigger_turn*/ false,
    );
    let serialized = serde_json::to_string(&communication).expect("communication should serialize");

    assert!(approx_token_count(&serialized) < COMPLETION_MESSAGE_MAX_TOKENS);
}
