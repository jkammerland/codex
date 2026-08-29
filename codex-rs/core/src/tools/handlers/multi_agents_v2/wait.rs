use super::*;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolSpec;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
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

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
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
            cancellation_token,
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
        let outcome = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => WaitOutcome::Interrupted,
            outcome = wait_for_activity(
                &session.services.agent_control,
                &turn.session_source,
                &current_agent_name,
                &mut activity_rx,
                pending_activity,
            ) => outcome?,
        };
        let result = WaitAgentResult::from_outcome(outcome);

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: if outcome == WaitOutcome::Interrupted {
                        CollabAgentToolCallStatus::Interrupted
                    } else {
                        CollabAgentToolCallStatus::Completed
                    },
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

        if outcome == WaitOutcome::Interrupted {
            return Err(FunctionCallError::RespondToModel(
                "wait_agent was cancelled".to_string(),
            ));
        }

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn handles_cancellation_cooperatively(&self) -> bool {
        true
    }

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
            WaitOutcome::Interrupted => "Wait interrupted by turn cancellation.",
        };
        Self {
            message: message.to_string(),
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
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
    Interrupted,
}

async fn wait_for_activity(
    agent_control: &crate::agent::control::AgentControl,
    session_source: &SessionSource,
    current_agent_name: &str,
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
) -> Result<WaitOutcome, FunctionCallError> {
    if let Some(activity) = pending_activity {
        return Ok(match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        });
    }
    loop {
        let agents = agent_control
            .list_agents(session_source, /*path_prefix*/ None)
            .await
            .map_err(collab_spawn_error)?;
        let mut status_receivers = Vec::new();
        for agent in agents {
            if agent.agent_name == current_agent_name
                || !matches!(
                    agent.agent_status,
                    AgentStatus::PendingInit | AgentStatus::Running
                )
            {
                continue;
            }
            let Ok(status_rx) = agent_control.subscribe_status(agent.thread_id).await else {
                continue;
            };
            if status_rx.has_changed().is_ok()
                && matches!(
                    *status_rx.borrow(),
                    AgentStatus::PendingInit | AgentStatus::Running
                )
            {
                status_receivers.push(status_rx);
            }
        }
        if status_receivers.is_empty() {
            match activity_rx.has_changed() {
                Ok(true) => {
                    return Ok(match *activity_rx.borrow_and_update() {
                        InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                        InputQueueActivity::Steer => WaitOutcome::Steered,
                    });
                }
                Ok(false) => {}
                Err(_) => {
                    return Err(FunctionCallError::Fatal(
                        "wait_agent activity channel closed".to_string(),
                    ));
                }
            }
            return Ok(WaitOutcome::NoLiveAgents);
        }

        let mut status_changes = status_receivers
            .iter_mut()
            .map(tokio::sync::watch::Receiver::changed)
            .collect::<FuturesUnordered<_>>();
        tokio::select! {
            biased;
            activity = activity_rx.changed() => {
                activity.map_err(|_| {
                    FunctionCallError::Fatal("wait_agent activity channel closed".to_string())
                })?;
                return Ok(match *activity_rx.borrow_and_update() {
                    InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
                    InputQueueActivity::Steer => WaitOutcome::Steered,
                });
            }
            _ = status_changes.next() => {}
        }
    }
}
