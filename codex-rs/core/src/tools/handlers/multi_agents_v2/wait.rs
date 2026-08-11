use super::*;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_tools::ToolSpec;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let _: WaitArgs = parse_arguments(&arguments)?;

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        session
            .services
            .agent_control
            .register_session_root(session.thread_id, turn.parent_thread_id);
        let current_agent_name = turn
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root)
            .to_string();
        let has_waitable_agent = if pending_activity.is_some() {
            true
        } else {
            session
                .services
                .agent_control
                .list_agents(&turn.session_source, /*path_prefix*/ None)
                .await
                .map_err(collab_spawn_error)?
                .iter()
                .any(|agent| {
                    agent.agent_name != current_agent_name
                        && !crate::agent::status::is_final(&agent.agent_status)
                })
        };
        let outcome = if has_waitable_agent {
            wait_for_activity(&mut activity_rx, pending_activity).await?
        } else {
            WaitOutcome::NoLiveAgents
        };
        let result = WaitAgentResult::from_outcome(outcome);

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
}

impl WaitAgentResult {
    fn from_outcome(outcome: WaitOutcome) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::NoLiveAgents => "No live agents to wait for.",
        };
        Self {
            message: message.to_string(),
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    MailboxActivity,
    Steered,
    NoLiveAgents,
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
) -> Result<WaitOutcome, FunctionCallError> {
    if let Some(activity) = pending_activity {
        return Ok(match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        });
    }
    activity_rx
        .changed()
        .await
        .map_err(|_| FunctionCallError::Fatal("wait_agent activity channel closed".to_string()))?;
    Ok(match *activity_rx.borrow_and_update() {
        InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
        InputQueueActivity::Steer => WaitOutcome::Steered,
    })
}
