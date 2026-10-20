use super::analytics::ToolCallAnalytics;
use super::message_tool::MessageArgs;
use super::message_tool::SendMessageArgs;
use super::message_tool::handle_message_string_tool;
use super::*;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::handlers::multi_agents_spec::create_send_message_tool;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    wake_enabled: bool,
}

impl Handler {
    pub(crate) fn new(wake_enabled: bool) -> Self {
        Self { wake_enabled }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        create_send_message_tool(self.wake_enabled)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let mut analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::SendMessage);
            let result = self.handle_call(invocation, &mut analytics).await;
            analytics.finish(&result);
            result
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
        analytics: &mut ToolCallAnalytics,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        // Keep parsing aligned with the feature-gated schema, including legacy diagnostics.
        let args: SendMessageArgs = if self.wake_enabled {
            parse_arguments(&arguments)?
        } else {
            let args: MessageArgs = parse_arguments(&arguments)?;
            SendMessageArgs {
                target: args.target,
                message: args.message,
                wake: None,
            }
        };
        let mode = match args.wake {
            Some(false) => MessageDeliveryMode::QueueOnly,
            Some(true) | None => MessageDeliveryMode::Auto,
        };
        handle_message_string_tool(invocation, mode, args.target, args.message, analytics).await
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
