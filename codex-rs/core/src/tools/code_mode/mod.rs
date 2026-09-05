mod cell_admission;
mod cell_validation;
mod deferred_inputs;
mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod live_exec_sessions;
mod response_adapter;
mod result;
mod telemetry;
mod trace_lifecycle;
mod wait_handler;
pub(crate) mod wait_spec;

#[cfg(test)]
#[path = "cell_id_tests.rs"]
mod cell_id_tests;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use futures::future::join_all;
use serde_json::Value as JsonValue;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::config::CodeModeConfig;
use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail as sanitize_image_detail_items;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::ExecutedToolCallRecorder;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;

use deferred_inputs::AdmittedCellInputBatch;
use deferred_inputs::DeferredCellInput;
use deferred_inputs::DeferredCellInputRegistry;
use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use live_exec_sessions::LiveExecSessionRegistry;
use response_adapter::into_function_call_output_content_items;
use result::CodeModeOutputKind;
use result::CodeModeResultEnvelope;
pub(crate) use result::bound_prepared_code_mode_output;
use result::finalize_code_mode_result;
use result::host_failure_status;
use result::script_status;
use trace_lifecycle::CellTraceProvenance;
pub(crate) use wait_handler::CodeModeWaitHandler;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
pub(crate) const DEFAULT_WAIT_YIELD_TIME_MS: u64 = codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS;

/// Returns true for the code-mode `exec` tool in the default namespace.
pub(crate) fn is_exec_tool_name(tool_name: &ToolName) -> bool {
    tool_name.is_default_namespace() && tool_name.name == PUBLIC_TOOL_NAME
}

pub(crate) fn validate_core_tool_call_id(
    tool_name: &ToolName,
    call_id: &str,
) -> Result<(), String> {
    cell_validation::validate_core_tool_call_id(tool_name, call_id)
}

#[derive(Clone)]
pub(crate) struct ExecContext {
    pub(super) session: Arc<Session>,
    pub(super) turn: Arc<TurnContext>,
}

pub(crate) struct CodeModeService {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    session_provider: Arc<dyn CodeModeSessionProvider>,
    availability: Result<(), String>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    deferred_cell_inputs: Arc<DeferredCellInputRegistry>,
    live_exec_sessions: LiveExecSessionRegistry,
    trace_provenance: Mutex<CellTraceProvenance>,
    default_exec_yield_time_ms: u64,
    shutdown_token: CancellationToken,
    unavailable_warning_emitted: AtomicBool,
}

#[derive(Clone, Copy)]
enum ObservationFailureDisposition {
    Recoverable,
    Terminal,
}

impl CodeModeService {
    pub(crate) fn new(
        session_provider: Arc<dyn CodeModeSessionProvider>,
        config: &CodeModeConfig,
        executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>,
    ) -> Self {
        let deferred_cell_inputs = Arc::new(DeferredCellInputRegistry::default());
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::with_deferred_cell_inputs(
            executed_tool_calls,
            Arc::clone(&deferred_cell_inputs),
        ));
        let availability = session_provider.availability();
        Self {
            session: OnceCell::new(),
            session_provider,
            availability,
            dispatch_broker,
            deferred_cell_inputs,
            live_exec_sessions: LiveExecSessionRegistry::default(),
            trace_provenance: Mutex::new(CellTraceProvenance::default()),
            default_exec_yield_time_ms: config.default_exec_yield_time_ms,
            shutdown_token: CancellationToken::new(),
            unavailable_warning_emitted: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.availability.is_ok()
    }

    pub(crate) fn take_unavailable_warning(&self, tool_mode: ToolMode) -> Option<String> {
        let error = self.availability.as_ref().err()?;
        let behavior = match tool_mode {
            ToolMode::Direct => "Falling back to direct tools",
            ToolMode::CodeMode | ToolMode::CodeModeOnly => "Code mode will fail closed",
        };
        (!self
            .unavailable_warning_emitted
            .swap(true, Ordering::Relaxed))
        .then(|| {
            format!(
                "Code Mode is unavailable because {error}. {behavior}; enable `features.code_mode_host` and install `codex-code-mode-host`."
            )
        })
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    pub(crate) async fn execute(
        &self,
        mut request: codex_code_mode::ExecuteRequest,
    ) -> Result<codex_code_mode::StartedCell, String> {
        request
            .yield_time_ms
            .get_or_insert(self.default_exec_yield_time_ms);
        let admission = self.dispatch_broker.begin_cell_admission()?;
        let session = self.session().await?;
        let started = session.execute(request).await?;
        if let Err(error) = cell_validation::validate_host_cell_id(&started.cell_id) {
            terminate_rejected_host_cell(session.as_ref(), &started.cell_id).await;
            return Err(error);
        }
        if let Err(error) = self.dispatch_broker.admit_cell(admission, &started.cell_id) {
            terminate_rejected_host_cell(session.as_ref(), &started.cell_id).await;
            return Err(error);
        }
        let cell_id = started.cell_id.clone();
        let expected_cell_id = cell_id.clone();
        let dispatch_broker = Arc::clone(&self.dispatch_broker);
        let cleanup_session = Arc::clone(&session);
        Ok(codex_code_mode::StartedCell::from_future(
            cell_id.clone(),
            async move {
                let response = started.initial_response().await?;
                if let Err(error) = cell_validation::validate_runtime_response_for_cell(
                    &expected_cell_id,
                    &response,
                ) {
                    terminate_rejected_host_cell(cleanup_session.as_ref(), &cell_id).await;
                    dispatch_broker.close_cell(&cell_id);
                    return Err(error);
                }
                Ok(response)
            },
        ))
    }

    pub(crate) async fn wait(
        &self,
        request: codex_code_mode::WaitRequest,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        let expected_cell_id = request.cell_id.clone();
        let session = self.session().await?;
        let outcome = session.wait(request).await?;
        if let Err(error) =
            cell_validation::validate_wait_outcome_for_cell(&expected_cell_id, &outcome)
        {
            terminate_rejected_host_cell(session.as_ref(), &expected_cell_id).await;
            self.dispatch_broker.close_cell(&expected_cell_id);
            return Err(error);
        }
        Ok(outcome)
    }

    pub(crate) async fn terminate(
        &self,
        cell_id: CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        let expected_cell_id = cell_id.clone();
        let outcome = self.session().await?.terminate(cell_id).await?;
        if let Err(error) =
            cell_validation::validate_wait_outcome_for_cell(&expected_cell_id, &outcome)
        {
            self.dispatch_broker.close_cell(&expected_cell_id);
            return Err(error);
        }
        Ok(outcome)
    }

    pub(crate) async fn interrupt_active_cells(&self) {
        let Some(session) = self.session.get() else {
            return;
        };
        join_all(
            self.dispatch_broker
                .active_cell_ids()
                .into_iter()
                .map(|cell_id| async move {
                    if let Err(error) = session.terminate(cell_id.clone()).await {
                        tracing::warn!(%cell_id, %error, "failed to terminate interrupted code-mode cell");
                    }
                }),
        )
        .await;
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutdown_token.cancel();
        self.dispatch_broker.shutdown();
        self.deferred_cell_inputs.clear();
        self.trace_provenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // Join any initialization already in progress without initializing an unused service.
        match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => session.shutdown().await,
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn mark_cell_ready_for_dispatch(
        &self,
        cell_id: &codex_code_mode::CellId,
        originating_item_id: Option<codex_protocol::ResponseItemId>,
    ) {
        self.dispatch_broker
            .mark_cell_ready_for_dispatch(cell_id, originating_item_id);
    }

    pub(crate) fn cell_originating_item_id(
        &self,
        cell_id: &codex_code_mode::CellId,
    ) -> Option<codex_protocol::ResponseItemId> {
        self.dispatch_broker.cell_originating_item_id(cell_id)
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
    }

    pub(crate) fn record_cell_trace_started(&self, cell_id: &CellId) {
        self.trace_provenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_started(cell_id);
    }

    pub(crate) fn claim_cell_trace_end(&self, cell_id: &CellId) -> bool {
        self.trace_provenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .claim_end(cell_id)
    }

    fn observe_nested_exec_result(
        &self,
        cell_id: &CellId,
        tool_name: &str,
        is_default_namespace: bool,
        input_session_id: Option<i32>,
        result: &JsonValue,
    ) {
        self.live_exec_sessions.observe_tool_result(
            cell_id,
            tool_name,
            is_default_namespace,
            input_session_id,
            result,
        );
    }

    fn live_exec_sessions_for_cell(&self, cell_id: &CellId) -> Vec<i32> {
        self.live_exec_sessions.sessions_for_cell(cell_id)
    }

    pub(crate) fn admit_cell_notification(
        &self,
        session: &Session,
        turn: &TurnContext,
        cell_id: &CellId,
        call_id: String,
        text: String,
    ) -> Option<AdmittedCellInputBatch> {
        let input = prepare_deferred_cell_input(
            session,
            turn,
            DeferredCellInput::notification(call_id, text),
        )?;
        self.deferred_cell_inputs
            .admit_inputs(cell_id, std::iter::once(input))
    }

    pub(crate) fn admit_cell_hook_contexts(
        &self,
        session: &Session,
        turn: &TurnContext,
        cell_id: &CellId,
        contexts: Vec<String>,
    ) -> Option<AdmittedCellInputBatch> {
        let inputs = contexts.into_iter().filter_map(|text| {
            prepare_deferred_cell_input(session, turn, DeferredCellInput::hook_context(text))
        });
        self.deferred_cell_inputs.admit_inputs(cell_id, inputs)
    }

    pub(crate) fn defer_admitted_cell_inputs(&self, batch: AdmittedCellInputBatch) {
        self.deferred_cell_inputs.defer_admitted(batch);
    }

    pub(crate) fn acknowledge_sampled_cell_inputs(
        &self,
        input: &[codex_protocol::models::ResponseItem],
        turn_id: &str,
    ) {
        self.deferred_cell_inputs
            .acknowledge_model_sample(input, turn_id);
    }

    fn subscribe_deferred_cell_input_activity(
        &self,
        cell_id: &CellId,
    ) -> (tokio::sync::watch::Receiver<u64>, bool) {
        let activity_rx = self.deferred_cell_inputs.subscribe_activity();
        let pending = self.deferred_cell_inputs.has_pending_for_cell(cell_id);
        (activity_rx, pending)
    }

    fn has_deferred_cell_inputs(&self, cell_id: &CellId) -> bool {
        self.deferred_cell_inputs.has_pending_for_cell(cell_id)
    }

    async fn inject_deferred_inputs_for_cell(&self, exec: &ExecContext, cell_id: &CellId) {
        let Some(batch) = self.deferred_cell_inputs.take_admitted(cell_id) else {
            return;
        };
        if exec
            .session
            .inject_if_running_for_turn(&exec.turn.sub_id, batch.response_items())
            .await
            .is_err()
        {
            self.deferred_cell_inputs.restore(batch);
        }
    }

    pub(crate) fn start_turn_worker(
        &self,
        session: &Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Option<CodeModeDispatchWorker> {
        let turn = &step_context.turn;
        if !step_context.tool_router.requires_code_mode_worker() {
            return None;
        }

        let exec = ExecContext {
            session: Arc::clone(session),
            turn: Arc::clone(turn),
        };
        Some(
            self.dispatch_broker
                .start_turn_worker(exec, step_context, tracker),
        )
    }

    pub(crate) async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutdown_token.is_cancelled() {
            return Err("code mode session is shutting down".to_string());
        }
        self.session
            .get_or_try_init(|| async {
                if self.shutdown_token.is_cancelled() {
                    return Err("code mode session is shutting down".to_string());
                }
                let session = tokio::select! {
                    biased;
                    _ = self.shutdown_token.cancelled() => {
                        return Err("code mode session is shutting down".to_string());
                    }
                    session = self
                        .session_provider
                        .create_session(self.dispatch_broker.clone()) => session?,
                };
                if self.shutdown_token.is_cancelled() {
                    let _ = session.shutdown().await;
                    return Err("code mode session is shutting down".to_string());
                }
                Ok(session)
            })
            .await
            .map(Arc::clone)
    }
}

fn prepare_deferred_cell_input(
    session: &Session,
    turn: &TurnContext,
    input: DeferredCellInput,
) -> Option<DeferredCellInput> {
    let response_item = input.response_item();
    let (prepared, _) =
        session.prepare_conversation_items_for_history(turn, std::slice::from_ref(&response_item));
    let [prepared] = prepared.as_ref() else {
        tracing::warn!("history preparation changed the deferred code-mode input item count");
        return None;
    };
    input.with_prepared_response_item(prepared.clone())
}

async fn terminate_rejected_host_cell(session: &dyn CodeModeSession, cell_id: &CellId) {
    if let Err(error) = session.terminate(cell_id.clone()).await {
        tracing::warn!(
            %cell_id,
            %error,
            "failed to terminate rejected code-mode cell"
        );
    }
}

async fn handle_runtime_response(
    exec: &ExecContext,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
    call_id: &str,
    output_kind: CodeModeOutputKind,
) -> Result<FunctionToolOutput, String> {
    let script_status = script_status(&response)?;
    let cell_id = match &response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id,
    };
    exec.session
        .services
        .code_mode_service
        .inject_deferred_inputs_for_cell(exec, cell_id)
        .await;
    let observed_live_exec_sessions = match &response {
        RuntimeResponse::Yielded { .. } => Vec::new(),
        RuntimeResponse::Terminated { cell_id, .. } | RuntimeResponse::Result { cell_id, .. } => {
            exec.session
                .services
                .code_mode_service
                .live_exec_sessions_for_cell(cell_id)
        }
    };
    let live_exec_sessions = exec
        .session
        .services
        .unified_exec_manager
        .retain_existing_process_ids(observed_live_exec_sessions)
        .await;

    match response {
        RuntimeResponse::Yielded { content_items, .. } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            let response_envelope =
                CodeModeResultEnvelope::new(call_id, exec.turn.sub_id.as_str(), output_kind, true);
            content_items = finalize_code_mode_result(
                content_items,
                max_output_tokens,
                &script_status,
                wall_time,
                &live_exec_sessions,
                &response_envelope,
            );
            Ok(FunctionToolOutput::from_content(content_items, Some(true)))
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            let response_envelope =
                CodeModeResultEnvelope::new(call_id, exec.turn.sub_id.as_str(), output_kind, true);
            content_items = finalize_code_mode_result(
                content_items,
                max_output_tokens,
                &script_status,
                wall_time,
                &live_exec_sessions,
                &response_envelope,
            );
            Ok(FunctionToolOutput::from_content(content_items, Some(true)))
        }
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            let success = error_text.is_none();
            if let Some(error_text) = error_text {
                content_items.push(FunctionCallOutputContentItem::InputText {
                    text: format!("Script error:\n{error_text}"),
                });
            }
            let response_envelope = CodeModeResultEnvelope::new(
                call_id,
                exec.turn.sub_id.as_str(),
                output_kind,
                success,
            );
            content_items = finalize_code_mode_result(
                content_items,
                max_output_tokens,
                &script_status,
                wall_time,
                &live_exec_sessions,
                &response_envelope,
            );
            Ok(FunctionToolOutput::from_content(
                content_items,
                Some(success),
            ))
        }
    }
}

async fn handle_runtime_failure(
    exec: &ExecContext,
    cell_id: &CellId,
    error: String,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
    call_id: &str,
    output_kind: CodeModeOutputKind,
) -> FunctionToolOutput {
    let failure_disposition = if exec
        .session
        .services
        .code_mode_service
        .dispatch_broker
        .cell_is_active(cell_id)
    {
        ObservationFailureDisposition::Recoverable
    } else {
        ObservationFailureDisposition::Terminal
    };
    let status = match failure_disposition {
        ObservationFailureDisposition::Recoverable => script_status(&RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
            reason: codex_code_mode::YieldReason::Requested,
            code_mode_host_duration: None,
        })
        .unwrap_or_else(|_| host_failure_status()),
        ObservationFailureDisposition::Terminal => {
            let terminal_response = RuntimeResponse::Result {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
                error_text: Some(error.clone()),
                code_mode_host_duration: None,
            };
            let trace_end_claimed = exec
                .session
                .services
                .code_mode_service
                .claim_cell_trace_end(cell_id);
            if trace_end_claimed {
                exec.session
                    .services
                    .rollout_thread_trace
                    .code_cell_trace_context(exec.turn.sub_id.as_str(), cell_id.as_str())
                    .record_ended(&terminal_response);
            }
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(cell_id);
            if trace_end_claimed {
                exec.session
                    .services
                    .analytics_events_client
                    .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::CellClosed {
                        thread_id: exec.session.thread_id.to_string(),
                        turn_id: exec.turn.sub_id.clone(),
                        cell_id: cell_id.to_string(),
                    });
            }
            host_failure_status()
        }
    };
    exec.session
        .services
        .code_mode_service
        .inject_deferred_inputs_for_cell(exec, cell_id)
        .await;
    let observed_live_exec_sessions = exec
        .session
        .services
        .code_mode_service
        .live_exec_sessions_for_cell(cell_id);
    let live_exec_sessions = exec
        .session
        .services
        .unified_exec_manager
        .retain_existing_process_ids(observed_live_exec_sessions)
        .await;
    let response_envelope =
        CodeModeResultEnvelope::new(call_id, exec.turn.sub_id.as_str(), output_kind, false);
    let content_items = finalize_code_mode_result(
        vec![FunctionCallOutputContentItem::InputText {
            text: format!("Code-mode host error:\n{error}"),
        }],
        max_output_tokens,
        &status,
        wall_time,
        &live_exec_sessions,
        &response_envelope,
    );
    FunctionToolOutput::from_content(content_items, Some(false))
}

fn sanitize_runtime_image_detail(turn: &TurnContext, items: &mut [FunctionCallOutputContentItem]) {
    sanitize_image_detail_items(can_request_original_image_detail(turn.model_info()), items);
}

// Submit synchronously so the recorder sees the call before the cell's dispatch gate closes.
fn submit_nested_tool(
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    invocation: CodeModeNestedToolCall,
    cancellation_token: CancellationToken,
) -> Result<
    impl std::future::Future<Output = Result<JsonValue, FunctionCallError>> + Send + 'static,
    FunctionCallError,
> {
    let CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
    } = invocation;
    let nested_tool_name = tool_name.name.clone();
    let is_default_namespace = tool_name.is_default_namespace();
    let input_session_id = input
        .as_ref()
        .and_then(|input| input.get("session_id"))
        .and_then(JsonValue::as_i64)
        .and_then(|session_id| i32::try_from(session_id).ok());
    if is_exec_tool_name(&tool_name) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{PUBLIC_TOOL_NAME} cannot invoke itself"
        )));
    }

    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => return Err(FunctionCallError::RespondToModel(error)),
    };

    let call = ToolCall {
        tool_name: tool_name.with_default_namespace(),
        call_id: format!("{PUBLIC_TOOL_NAME}-{}", uuid::Uuid::new_v4()),
        payload,
        encrypted_function_args: None,
    };
    exec.session
        .services
        .analytics_events_client
        .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::ChildStarted {
            thread_id: exec.session.thread_id.to_string(),
            turn_id: exec.turn.sub_id.clone(),
            call_id: call.call_id.clone(),
            cell_id: cell_id.to_string(),
        });
    let result = tool_runtime.handle_tool_call_with_source(
        call,
        ToolCallSource::CodeMode {
            cell_id: cell_id.to_string(),
            runtime_tool_call_id,
        },
        cancellation_token,
    );
    Ok(async move {
        let result = result.await?.code_mode_result();
        exec.session
            .services
            .code_mode_service
            .observe_nested_exec_result(
                &cell_id,
                nested_tool_name.as_str(),
                is_default_namespace,
                input_session_id,
                &result,
            );
        Ok(result)
    })
}

fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function => build_function_tool_payload(tool_name, input),
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn build_function_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    Ok(ToolPayload::Function { arguments })
}

fn serialize_function_tool_arguments(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<String, String> {
    match input {
        None => Ok("{}".to_string()),
        Some(JsonValue::Object(map)) => serde_json::to_string(&JsonValue::Object(map))
            .map_err(|err| format!("failed to serialize tool `{tool_name}` arguments: {err}")),
        Some(_) => Err(format!(
            "tool `{tool_name}` expects a JSON object for arguments"
        )),
    }
}

fn build_freeform_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match input {
        Some(JsonValue::String(input)) => Ok(ToolPayload::Custom { input }),
        _ => Err(format!("tool `{tool_name}` expects a string input")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::build_nested_tool_payload;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolRouter;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_code_mode::CodeModeToolKind;
    use codex_protocol::openai_models::ToolMode;
    use codex_tools::ToolName;
    use serde_json::json;

    #[tokio::test]
    async fn turn_worker_uses_step_router_mode_instead_of_admitted_turn() {
        let (session, turn) = make_session_and_context().await;
        assert_eq!(
            crate::tools::effective_tool_mode(&turn, turn.model_info()),
            ToolMode::Direct
        );
        let session = Arc::new(session);
        let step_context = StepContext::for_test(Arc::new(turn));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::empty_for_test(),
            Vec::new(),
            ToolMode::CodeModeOnly,
            BTreeMap::new(),
            /*tool_namespaces_info*/ None,
            &[],
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let worker =
            session
                .services
                .code_mode_service
                .start_turn_worker(&session, step_context, tracker);

        assert!(worker.is_some());
    }

    #[test]
    fn build_nested_tool_payload_uses_function_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("example"),
            Some(json!({ "value": 1 })),
        )
        .expect("function payload should serialize");

        match payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, r#"{"value":1}"#.to_string());
            }
            other => panic!("expected function payload, got {other:?}"),
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_freeform_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Freeform,
            &ToolName::plain("example"),
            Some(json!("hello")),
        )
        .expect("freeform payload should preserve string input");

        match payload {
            ToolPayload::Custom { input } => {
                assert_eq!(input, "hello".to_string());
            }
            other => panic!("expected freeform payload, got {other:?}"),
        }
    }
}
