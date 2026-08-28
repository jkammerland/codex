use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::UnifiedExecInteractionEvent;
use crate::unified_exec::WaitProcessRequest;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::super::shell_spec::create_wait_process_tool;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitProcessArgs {
    session_id: i32,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub struct WaitProcessHandler;

impl ToolExecutor<ToolInvocation> for WaitProcessHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_process")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_process_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "wait_process handler received unsupported payload".to_string(),
                ));
            };
            let args: WaitProcessArgs = parse_arguments(&arguments)?;
            let turn_state = session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await;
            let (mut activity_rx, pending_activity) = session
                .input_queue
                .subscribe_activity(turn_state.as_deref())
                .await;

            let response = session
                .services
                .unified_exec_manager
                .wait_process(WaitProcessRequest {
                    process_id: args.session_id,
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: turn.model_info().truncation_policy.into(),
                    interaction_event: Some(UnifiedExecInteractionEvent {
                        session: &session,
                        turn: &turn,
                    }),
                    activity_rx: &mut activity_rx,
                    pending_activity,
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("wait_process failed: {err}"))
                })?;

            Ok(boxed_tool_output(response))
        })
    }
}

impl CoreToolRuntime for WaitProcessHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        None
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}
