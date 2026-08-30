use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::submit_nested_tool;
use crate::session::step_context::StepContext;
use crate::tools::ExecutedToolCallRecorder;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    dispatch_gates: Arc<Mutex<HashMap<CellId, CellDispatchGate>>>,
    host_state: Arc<Mutex<DispatchHostState>>,
    next_host_generation: AtomicU64,
    dispatcher_started: AtomicBool,
    dispatcher_shutdown: CancellationToken,
    executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>,
}

struct DispatchHostState {
    host: Option<Arc<CoreTurnHost>>,
    active_generation: Option<u64>,
}

struct CellDispatchGate {
    ready: watch::Sender<bool>,
    host: Option<Arc<CoreTurnHost>>,
    // Keep the original exec item when later waits resume this cell.
    originating_item_id: Option<ResponseItemId>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new(executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>) -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            dispatch_tx,
            dispatch_rx,
            dispatch_gates: Arc::new(Mutex::new(HashMap::new())),
            host_state: Arc::new(Mutex::new(DispatchHostState {
                host: None,
                active_generation: None,
            })),
            next_host_generation: AtomicU64::new(0),
            dispatcher_started: AtomicBool::new(false),
            dispatcher_shutdown: CancellationToken::new(),
            executed_tool_calls,
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(
        &self,
        cell_id: &CellId,
        originating_item_id: Option<ResponseItemId>,
    ) {
        let host = self
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .host
            .clone();
        let ready = {
            let mut dispatch_gates = self
                .dispatch_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let gate = dispatch_gates
                .entry(cell_id.clone())
                .or_insert_with(|| CellDispatchGate {
                    ready: watch::channel(false).0,
                    host: None,
                    originating_item_id: None,
                });
            gate.host = host;
            gate.originating_item_id = originating_item_id;
            gate.ready.clone()
        };
        ready.send_replace(true);
    }

    pub(super) fn cell_originating_item_id(&self, cell_id: &CellId) -> Option<ResponseItemId> {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id)
            .and_then(|gate| gate.originating_item_id.clone())
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        let no_active_cells = {
            let mut dispatch_gates = self
                .dispatch_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            dispatch_gates.remove(cell_id);
            dispatch_gates.is_empty()
        };
        if no_active_cells {
            let mut host_state = self
                .host_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if host_state.active_generation.is_none() {
                host_state.host = None;
            }
        }
        if let Some(recorder) = &self.executed_tool_calls {
            recorder.finish_cell_recording(cell_id);
        }
    }

    pub(super) fn active_cell_ids(&self) -> Vec<CellId> {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    pub(super) fn start_turn_worker(
        &self,
        exec: ExecContext,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> CodeModeDispatchWorker {
        let track_completeness = exec
            .turn
            .config
            .features
            .enabled(codex_features::Feature::ExecutedToolCallMetadata);
        let tool_runtime = ToolCallRuntime::new(Arc::clone(&exec.session), step_context, tracker);
        let host = Arc::new(CoreTurnHost {
            exec,
            tool_runtime,
            track_completeness,
        });
        let generation = self.next_host_generation.fetch_add(1, Ordering::Relaxed);
        if self.dispatcher_shutdown.is_cancelled() {
            return CodeModeDispatchWorker {
                dispatch_gates: Arc::clone(&self.dispatch_gates),
                host_state: Arc::clone(&self.host_state),
                generation,
            };
        }
        {
            let mut host_state = self
                .host_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            host_state.host = Some(host);
            host_state.active_generation = Some(generation);
        }
        if self
            .dispatcher_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let dispatch_rx = self.dispatch_rx.clone();
            let dispatch_gates = Arc::clone(&self.dispatch_gates);
            let dispatcher_shutdown = self.dispatcher_shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let message = tokio::select! {
                        biased;
                        _ = dispatcher_shutdown.cancelled() => break,
                        message = dispatch_rx.recv() => message.ok(),
                    };
                    let Some(message) = message else {
                        break;
                    };
                    match message {
                        DispatchMessage::Notify {
                            call_id,
                            cell_id,
                            text,
                            cancellation_token,
                            response_tx,
                        } => {
                            let response = if wait_until_cell_ready_for_dispatch(
                                &dispatch_gates,
                                &cell_id,
                                &cancellation_token,
                            )
                            .await
                            {
                                match dispatch_host(&dispatch_gates, &cell_id) {
                                    Some(host) => host.notify(call_id, cell_id, text).await,
                                    None => Err(format!(
                                        "code-mode cell {cell_id} has no originating dispatch host"
                                    )),
                                }
                            } else {
                                remove_dispatch_gate(&dispatch_gates, &cell_id);
                                Err("code mode notification cancelled".to_string())
                            };
                            let _ = response_tx.send(response);
                        }
                        DispatchMessage::InvokeTool {
                            invocation,
                            cancellation_token,
                            response_tx,
                            span,
                        } => {
                            let cell_id = invocation.cell_id.clone();
                            if !wait_until_cell_ready_for_dispatch(
                                &dispatch_gates,
                                &cell_id,
                                &cancellation_token,
                            )
                            .await
                            {
                                remove_dispatch_gate(&dispatch_gates, &cell_id);
                                continue;
                            }
                            let Some(host) = dispatch_host(&dispatch_gates, &cell_id) else {
                                let _ = response_tx.send(Err(format!(
                                    "code-mode cell {cell_id} has no originating dispatch host"
                                )));
                                continue;
                            };
                            let dispatch_gates = Arc::clone(&dispatch_gates);
                            tokio::spawn(async move {
                                let invocation = {
                                    let dispatch_gate = host.track_completeness.then(|| {
                                        dispatch_gates
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    });
                                    if dispatch_gate.as_ref().is_some_and(|gates| {
                                        cancellation_token.is_cancelled()
                                            || !gates.contains_key(&cell_id)
                                    }) {
                                        return;
                                    }
                                    // Submission and cell closure share this gate.
                                    span.in_scope(|| {
                                        host.submit_tool(invocation, cancellation_token.clone())
                                    })
                                    .instrument(span)
                                };
                                tokio::pin!(invocation);
                                let response = tokio::select! {
                                    biased;
                                    _ = cancellation_token.cancelled() => invocation.await,
                                    response = &mut invocation => response,
                                };
                                let _ = response_tx.send(response);
                            });
                        }
                    }
                }
            });
        }
        CodeModeDispatchWorker {
            dispatch_gates: Arc::clone(&self.dispatch_gates),
            host_state: Arc::clone(&self.host_state),
            generation,
        }
    }

    pub(super) fn shutdown(&self) {
        self.dispatcher_shutdown.cancel();
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let mut host_state = self
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host_state.host = None;
        host_state.active_generation = None;
    }
}

fn dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) -> watch::Sender<bool> {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates
        .entry(cell_id.clone())
        .or_insert_with(|| CellDispatchGate {
            ready: watch::channel(false).0,
            host: None,
            originating_item_id: None,
        })
        .ready
        .clone()
}

fn dispatch_host(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) -> Option<Arc<CoreTurnHost>> {
    dispatch_gates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(cell_id)
        .and_then(|gate| gate.host.clone())
}

fn remove_dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates.remove(cell_id);
}

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let mut ready_rx = dispatch_gate(dispatch_gates, cell_id).subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                    span: tracing::Span::current(),
                })
                .await
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.close_cell(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
        span: tracing::Span,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

pub(crate) struct CodeModeDispatchWorker {
    dispatch_gates: Arc<Mutex<HashMap<CellId, CellDispatchGate>>>,
    host_state: Arc<Mutex<DispatchHostState>>,
    generation: u64,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        let no_active_cells = self
            .dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        let mut host_state = self
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if host_state.active_generation == Some(self.generation) {
            host_state.active_generation = None;
            if no_active_cells {
                host_state.host = None;
            }
        }
    }
}

struct CoreTurnHost {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    track_completeness: bool,
}

impl CoreTurnHost {
    fn submit_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<JsonValue, String>> + Send + 'static {
        let invocation = submit_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation_token,
        )
        .map_err(|error| error.to_string());
        async move { invocation?.await.map_err(|error| error.to_string()) }
    }

    async fn notify(&self, call_id: String, cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_if_running(vec![ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .map_err(|_| {
                format!("failed to inject exec notify message for cell {cell_id}: no active turn")
            })
    }
}
