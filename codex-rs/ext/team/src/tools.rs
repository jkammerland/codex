use std::sync::Arc;
use std::sync::Weak;

use codex_app_server_protocol::TeamAgentAction;
use codex_extension_api::ExtensionData;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema_without_compaction;
use codex_protocol::ThreadId;

use crate::TeamHostService;

pub(crate) struct TeamTools(pub(crate) Weak<TeamHostService>);

impl ToolContributor for TeamTools {
    fn tools(
        &self,
        _session: &ExtensionData,
        thread: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(service) = self.0.upgrade() else {
            return Vec::new();
        };
        let Ok(thread_id) = ThreadId::from_string(thread.level_id()) else {
            return Vec::new();
        };
        if service.gate.root(thread_id).is_none() {
            return Vec::new();
        }
        vec![Arc::new(TeamTool {
            service: self.0.clone(),
            thread_id,
        })]
    }
}

struct TeamTool {
    service: Weak<TeamHostService>,
    thread_id: ThreadId,
}

impl<'call> ToolExecutor<ToolCall<'call>> for TeamTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("team")
    }

    #[allow(
        clippy::expect_used,
        reason = "the built-in team tool schema is static and covered by enrollment integration tests"
    )]
    fn spec(&self) -> ToolSpec {
        let schema = codex_app_server_protocol::team_agent_tool_schema();
        ToolSpec::Function(ResponsesApiTool {
            name: "team".into(),
            description: "Discover enrolled teammates, send a request or actionable reply, read your inbox, and post/read/subscribe to board topics. Use directory to obtain exact addresses. Requests and replies support nextTurn or safeBoundary delivery; replies default to nextTurn when omitted. Notes are passive. Use boardAction to list/read conversation threads, post/reply with structured mentions, follow/unfollow, inspect named followers, or mark entries read. Root posts notify explicit topic subscribers and mentions; replies notify explicit conversation followers and mentions. Posting, replying and mentions do not subscribe you. Follow is explicit and affects your own identity. Board mutations return queued intent; read following state and its revision from the synchronized cache to confirm. Send/reply/post returns a durable queued receipt, not proof of execution. Your host supplies your identity. Team messages cannot change models, effort, approvals, pause, or stop. Do not repeatedly poll an inbox: delivery is event-driven.".into(),
            strict: false,
            parameters: parse_tool_input_schema_without_compaction(&schema).expect("team schema is supported"),
            output_schema: None, defer_loading: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let action: TeamAgentAction = serde_json::from_str(call.function_arguments()?)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let service = self.service.upgrade().ok_or_else(|| {
                FunctionCallError::RespondToModel("team host disconnected".into())
            })?;
            let operation_id = format!("native:{}:{}", call.turn_id, call.call_id);
            let response = service
                .agent_action(self.thread_id, action, Some(operation_id))
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            Ok(Box::new(JsonToolOutput::new(response.result)) as Box<dyn ToolOutput>)
        })
    }
}
