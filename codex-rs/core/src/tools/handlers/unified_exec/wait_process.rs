use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolEnvironments;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::LiveEnvironmentResolver;
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
    // The live resolver captures the terminal; retain strict argument validation here.
    #[serde(rename = "session_id")]
    _session_id: i32,
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
                step_context,
                payload,
                environments,
                ..
            } = invocation;
            let ToolEnvironments::Terminal(terminal) = environments else {
                return Err(FunctionCallError::Fatal(
                    "wait_process requires a terminal session".to_string(),
                ));
            };
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
                    terminal: &terminal,
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: step_context.settings.model_info.truncation_policy.into(),
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
    fn live_environment_resolver(&self) -> Option<LiveEnvironmentResolver> {
        Some(super::terminal_session_for_input)
    }

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
