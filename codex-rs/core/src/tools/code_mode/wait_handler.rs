use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::handle_runtime_failure;
use super::handle_runtime_response;
use super::result::CodeModeOutputKind;
use super::telemetry::CodeModeToolCallGuard;
use super::wait_spec::create_wait_tool;

pub struct CodeModeWaitHandler;

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

enum WaitWake {
    Completed(Result<codex_code_mode::WaitOutcome, String>),
    Input(Result<(), tokio::sync::watch::error::RecvError>),
    DeferredInput(Result<(), tokio::sync::watch::error::RecvError>),
}

fn default_wait_yield_time_ms() -> u64 {
    DEFAULT_WAIT_YIELD_TIME_MS
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

impl ToolExecutor<ToolInvocation> for CodeModeWaitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_wait_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeWaitHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let mut telemetry = CodeModeToolCallGuard::new(
            session.services.analytics_events_client.clone(),
            session.thread_id.to_string(),
            turn.sub_id.clone(),
            turn.turn_metadata_state.clone(),
            call_id.clone(),
            WAIT_TOOL_NAME,
        );
        let result = match payload {
            ToolPayload::Function { arguments }
                if tool_name.is_default_namespace()
                    && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments).inspect_err(|_error| {
                    telemetry.finish(/*success*/ false);
                })?;
                let exec = ExecContext { session, turn };
                let started_at = std::time::Instant::now();
                let cell_id = codex_code_mode::CellId::try_new(args.cell_id)
                    .map_err(FunctionCallError::RespondToModel)?;
                telemetry.cell_id = Some(cell_id.to_string());
                let wait_response = if args.terminate {
                    exec.session
                        .services
                        .code_mode_service
                        .terminate(cell_id.clone())
                        .await
                        .map_err(FunctionCallError::RespondToModel)
                } else {
                    let turn_state = exec
                        .session
                        .input_queue
                        .turn_state_for_sub_id(&exec.session.active_turn, &exec.turn.sub_id)
                        .await;
                    let (mut activity_rx, mut pending_activity) = exec
                        .session
                        .input_queue
                        .subscribe_activity(turn_state.as_deref())
                        .await;
                    let (mut deferred_input_rx, mut pending_deferred_input) = exec
                        .session
                        .services
                        .code_mode_service
                        .subscribe_deferred_cell_input_activity(&cell_id);
                    let mut yield_time_ms = args.yield_time_ms;
                    let mut cumulative_code_mode_host_duration = Some(std::time::Duration::ZERO);
                    let observation = loop {
                        let has_pending_activity = pending_activity.take().is_some()
                            || std::mem::take(&mut pending_deferred_input);
                        let (observation, external_wake) = if has_pending_activity {
                            (
                                exec.session
                                    .services
                                    .code_mode_service
                                    .wait(codex_code_mode::WaitRequest {
                                        cell_id: cell_id.clone(),
                                        yield_time_ms: 0,
                                    })
                                    .await
                                    .map_err(FunctionCallError::RespondToModel),
                                true,
                            )
                        } else {
                            let wake = {
                                let wait = exec.session.services.code_mode_service.wait(
                                    codex_code_mode::WaitRequest {
                                        cell_id: cell_id.clone(),
                                        yield_time_ms,
                                    },
                                );
                                tokio::pin!(wait);
                                tokio::select! {
                                    biased;
                                    result = &mut wait => WaitWake::Completed(result),
                                    changed = activity_rx.changed() => WaitWake::Input(changed),
                                    changed = deferred_input_rx.changed() => {
                                        WaitWake::DeferredInput(changed)
                                    }
                                }
                            };
                            match wake {
                                WaitWake::Completed(result) => {
                                    (result.map_err(FunctionCallError::RespondToModel), false)
                                }
                                WaitWake::Input(Ok(())) => (
                                    exec.session
                                        .services
                                        .code_mode_service
                                        .wait(codex_code_mode::WaitRequest {
                                            cell_id: cell_id.clone(),
                                            yield_time_ms: 0,
                                        })
                                        .await
                                        .map_err(FunctionCallError::RespondToModel),
                                    true,
                                ),
                                WaitWake::Input(Err(_)) => (
                                    Err(FunctionCallError::Fatal(
                                        "code-mode input activity channel closed".to_string(),
                                    )),
                                    true,
                                ),
                                WaitWake::DeferredInput(Ok(())) => {
                                    if !exec
                                        .session
                                        .services
                                        .code_mode_service
                                        .has_deferred_cell_inputs(&cell_id)
                                    {
                                        continue;
                                    }
                                    (
                                        exec.session
                                            .services
                                            .code_mode_service
                                            .wait(codex_code_mode::WaitRequest {
                                                cell_id: cell_id.clone(),
                                                yield_time_ms: 0,
                                            })
                                            .await
                                            .map_err(FunctionCallError::RespondToModel),
                                        true,
                                    )
                                }
                                WaitWake::DeferredInput(Err(_)) => (
                                    Err(FunctionCallError::Fatal(
                                        "code-mode deferred input activity channel closed"
                                            .to_string(),
                                    )),
                                    true,
                                ),
                            }
                        };
                        let observation = match observation {
                            Ok(observation) => observation,
                            Err(error) => break Err(error),
                        };
                        cumulative_code_mode_host_duration = match (
                            cumulative_code_mode_host_duration,
                            observation.code_mode_host_duration(),
                        ) {
                            (Some(cumulative), Some(duration)) => {
                                Some(cumulative.saturating_add(duration))
                            }
                            (Some(_) | None, None) | (None, Some(_)) => None,
                        };
                        if !external_wake
                            && matches!(
                                &observation,
                                codex_code_mode::WaitOutcome::LiveCell(
                                    codex_code_mode::RuntimeResponse::Yielded {
                                        content_items,
                                        reason: codex_code_mode::YieldReason::DeadlineElapsed,
                                        ..
                                    }
                                ) if content_items.is_empty()
                            )
                        {
                            yield_time_ms = args.yield_time_ms.max(MIN_EMPTY_YIELD_TIME_MS);
                            continue;
                        }
                        break Ok(observation);
                    };
                    observation.map(|observation| match cumulative_code_mode_host_duration {
                        Some(duration) => observation.with_code_mode_host_duration(duration),
                        None => observation,
                    })
                };
                let wait_response = match wait_response {
                    Ok(wait_response) => wait_response,
                    Err(FunctionCallError::RespondToModel(error)) => {
                        let output = handle_runtime_failure(
                            &exec,
                            &cell_id,
                            error,
                            args.max_tokens,
                            started_at.elapsed(),
                            &call_id,
                            CodeModeOutputKind::Wait,
                        )
                        .await;
                        telemetry.finish(/*success*/ false);
                        return Ok(boxed_tool_output(output));
                    }
                    Err(error) => {
                        telemetry.finish(/*success*/ false);
                        return Err(error);
                    }
                };
                let (response, missing_cell) = match &wait_response {
                    codex_code_mode::WaitOutcome::LiveCell(response) => (response, false),
                    codex_code_mode::WaitOutcome::MissingCell(response) => (response, true),
                };
                let runtime_cell_id = match response {
                    codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                    | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                    | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                };
                telemetry.cell_id = Some(runtime_cell_id.to_string());
                if !missing_cell
                    && let Some(executed_tool_calls) =
                        exec.session.services.executed_tool_calls.as_ref()
                {
                    executed_tool_calls.register_cell(runtime_cell_id, &call_id);
                }
                if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
                    let trace_end_claimed = exec
                        .session
                        .services
                        .code_mode_service
                        .claim_cell_trace_end(runtime_cell_id);
                    if trace_end_claimed {
                        exec.session
                            .services
                            .rollout_thread_trace
                            .code_cell_trace_context(
                                exec.turn.sub_id.as_str(),
                                runtime_cell_id.as_str(),
                            )
                            .record_ended(response);
                    }
                    exec.session
                        .services
                        .code_mode_service
                        .finish_cell_dispatch(runtime_cell_id);
                    if trace_end_claimed {
                        exec.session
                            .services
                            .analytics_events_client
                            .track_code_mode_tool_call(
                                codex_analytics::CodeModeToolCallFact::CellClosed {
                                    thread_id: exec.session.thread_id.to_string(),
                                    turn_id: exec.turn.sub_id.clone(),
                                    cell_id: runtime_cell_id.to_string(),
                                },
                            );
                    }
                }
                if let Some(code_mode_host_duration) = wait_response.code_mode_host_duration() {
                    telemetry.record_code_mode_host_duration(code_mode_host_duration);
                }
                exec.session.services.elicitations.wait_until_clear().await;
                let wall_time = wait_response
                    .code_mode_host_duration()
                    .unwrap_or_else(|| started_at.elapsed());
                handle_runtime_response(
                    &exec,
                    wait_response.into(),
                    args.max_tokens,
                    wall_time,
                    &call_id,
                    CodeModeOutputKind::Wait,
                )
                .await
                .map_err(FunctionCallError::RespondToModel)
                .map(boxed_tool_output)
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        };
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(codex_tools::ToolOutput::success_for_logging),
        );
        result
    }
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself.
        None
    }

    fn post_tool_use_payload(
        &self,
        _invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // The wait result feeds code-mode control flow, so do not let
        // PostToolUse replace it with model-facing hook feedback.
        None
    }
}
