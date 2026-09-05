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
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::ExecContext;
use super::cell_admission::CellAdmissionAttempt;
use super::cell_admission::CellAdmissionOutcome;
use super::cell_admission::CellAdmissionRegistry;
use super::cell_admission::CellAdmissionResolution;
use super::deferred_inputs::DeferredCellInputRegistry;
use super::submit_nested_tool;
use crate::session::step_context::StepContext;
use crate::tools::ExecutedToolCallRecorder;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

const INVOKE_DISPATCH_SHUTDOWN_ERROR: &str = "code mode nested tool dispatcher is shut down";
const NOTIFY_DISPATCH_SHUTDOWN_ERROR: &str = "code mode notification dispatcher is shut down";

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    cell_admissions: CellAdmissionRegistry,
    deferred_cell_inputs: Arc<DeferredCellInputRegistry>,
    dispatch_gates: Arc<Mutex<HashMap<CellId, CellDispatchGate>>>,
    host_state: Arc<Mutex<DispatchHostState>>,
    next_host_generation: AtomicU64,
    dispatcher_started: AtomicBool,
    dispatcher_shutdown: CancellationToken,
    executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>,
    #[cfg(test)]
    host_install_barrier: Mutex<Option<Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    nested_tool_submission_barrier: Arc<Mutex<Option<Arc<tokio::sync::Barrier>>>>,
    #[cfg(test)]
    readiness_wait_barrier: Arc<Mutex<Option<Arc<tokio::sync::Barrier>>>>,
    #[cfg(test)]
    nested_tool_submissions: Arc<AtomicU64>,
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
    #[cfg(test)]
    pub(super) fn new(executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>) -> Self {
        Self::with_deferred_cell_inputs(
            executed_tool_calls,
            Arc::new(DeferredCellInputRegistry::default()),
        )
    }

    pub(super) fn with_deferred_cell_inputs(
        executed_tool_calls: Option<Arc<ExecutedToolCallRecorder>>,
        deferred_cell_inputs: Arc<DeferredCellInputRegistry>,
    ) -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            dispatch_tx,
            dispatch_rx,
            cell_admissions: CellAdmissionRegistry::default(),
            deferred_cell_inputs,
            dispatch_gates: Arc::new(Mutex::new(HashMap::new())),
            host_state: Arc::new(Mutex::new(DispatchHostState {
                host: None,
                active_generation: None,
            })),
            next_host_generation: AtomicU64::new(0),
            dispatcher_started: AtomicBool::new(false),
            dispatcher_shutdown: CancellationToken::new(),
            executed_tool_calls,
            #[cfg(test)]
            host_install_barrier: Mutex::new(None),
            #[cfg(test)]
            nested_tool_submission_barrier: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            readiness_wait_barrier: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            nested_tool_submissions: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn begin_cell_admission(&self) -> Result<CellAdmissionAttempt, String> {
        self.cell_admissions.begin()
    }

    pub(super) fn admit_cell(
        &self,
        admission: CellAdmissionAttempt,
        cell_id: &CellId,
    ) -> Result<(), String> {
        cell_id
            .validate()
            .map_err(|error| format!("code-mode host returned invalid cell ID: {error}"))?;
        let outcome = admission.admit(cell_id, || {
            let mut dispatch_gates = self
                .dispatch_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if dispatch_gates.contains_key(cell_id) {
                return Err("code-mode host returned a cell ID that is already active".to_string());
            }
            dispatch_gates.insert(
                cell_id.clone(),
                CellDispatchGate {
                    ready: watch::channel(false).0,
                    host: None,
                    originating_item_id: None,
                },
            );
            self.deferred_cell_inputs.open_cell(cell_id);
            Ok(())
        })?;
        match outcome {
            CellAdmissionOutcome::Admitted => {}
            CellAdmissionOutcome::CloseRequested => self.close_cell(cell_id),
            CellAdmissionOutcome::Rejected => {
                return Err("code mode session is shutting down".to_string());
            }
        }
        Ok(())
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
            let Some(gate) = dispatch_gates.get_mut(cell_id) else {
                tracing::warn!("code-mode cell closed before its dispatch gate became ready");
                return;
            };
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
        self.deferred_cell_inputs.discard_cell(cell_id);
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
        let tool_runtime = ToolCallRuntime::new(Arc::clone(&exec.session), step_context, tracker);
        let host = Arc::new(CoreTurnHost {
            exec,
            tool_runtime,
            #[cfg(test)]
            nested_tool_submissions: Arc::clone(&self.nested_tool_submissions),
        });
        let generation = self.next_host_generation.fetch_add(1, Ordering::Relaxed);
        if self.dispatcher_shutdown.is_cancelled() {
            return CodeModeDispatchWorker {
                dispatch_gates: Arc::clone(&self.dispatch_gates),
                host_state: Arc::clone(&self.host_state),
                generation,
            };
        }
        #[cfg(test)]
        if let Some(barrier) = self
            .host_install_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            barrier.wait();
            barrier.wait();
        }
        let host_installed = {
            let mut host_state = self
                .host_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.dispatcher_shutdown.is_cancelled() {
                false
            } else {
                host_state.host = Some(host);
                host_state.active_generation = Some(generation);
                true
            }
        };
        if !host_installed {
            return CodeModeDispatchWorker {
                dispatch_gates: Arc::clone(&self.dispatch_gates),
                host_state: Arc::clone(&self.host_state),
                generation,
            };
        }
        if self
            .dispatcher_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let dispatch_rx = self.dispatch_rx.clone();
            let dispatch_gates = Arc::clone(&self.dispatch_gates);
            let dispatcher_shutdown = self.dispatcher_shutdown.clone();
            #[cfg(test)]
            let nested_tool_submission_barrier = Arc::clone(&self.nested_tool_submission_barrier);
            #[cfg(test)]
            let readiness_wait_barrier = Arc::clone(&self.readiness_wait_barrier);
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
                            let response = tokio::select! {
                                biased;
                                _ = dispatcher_shutdown.cancelled() => {
                                    Err(NOTIFY_DISPATCH_SHUTDOWN_ERROR.to_string())
                                }
                                response = async {
                                    if wait_until_cell_ready_for_dispatch(
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
                                        Err("code mode notification cancelled".to_string())
                                    }
                                } => response,
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
                            #[cfg(test)]
                            let readiness_barrier = readiness_wait_barrier
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            #[cfg(test)]
                            if let Some(barrier) = readiness_barrier {
                                barrier.wait().await;
                                barrier.wait().await;
                            }
                            let ready = tokio::select! {
                                biased;
                                _ = dispatcher_shutdown.cancelled() => {
                                    let _ = response_tx.send(Err(
                                        INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string()
                                    ));
                                    continue;
                                }
                                ready = wait_until_cell_ready_for_dispatch(
                                    &dispatch_gates,
                                    &cell_id,
                                    &cancellation_token,
                                ) => ready,
                            };
                            if !ready {
                                let _ = response_tx
                                    .send(Err("code mode nested tool call cancelled".to_string()));
                                continue;
                            }
                            let Some(host) = dispatch_host(&dispatch_gates, &cell_id) else {
                                let _ = response_tx.send(Err(format!(
                                    "code-mode cell {cell_id} has no originating dispatch host"
                                )));
                                continue;
                            };
                            let dispatch_gates = Arc::clone(&dispatch_gates);
                            let dispatcher_shutdown = dispatcher_shutdown.clone();
                            #[cfg(test)]
                            let submission_barrier = nested_tool_submission_barrier
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            tokio::spawn(async move {
                                #[cfg(test)]
                                if let Some(barrier) = submission_barrier {
                                    barrier.wait().await;
                                    barrier.wait().await;
                                }
                                let invocation = {
                                    let gates = dispatch_gates
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    let rejection = if dispatcher_shutdown.is_cancelled() {
                                        Some(INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string())
                                    } else if cancellation_token.is_cancelled() {
                                        Some("code mode nested tool call cancelled".to_string())
                                    } else if !gates.contains_key(&cell_id) {
                                        Some(format!(
                                            "code-mode cell {cell_id} is no longer active"
                                        ))
                                    } else {
                                        None
                                    };
                                    if let Some(error) = rejection {
                                        let _ = response_tx.send(Err(error));
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
                                    _ = dispatcher_shutdown.cancelled() => {
                                        cancellation_token.cancel();
                                        let _ = invocation.await;
                                        Err(INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string())
                                    }
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
        self.dispatch_tx.close();
        self.dispatcher_shutdown.cancel();
        self.cell_admissions.reject_all();
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        while let Ok(message) = self.dispatch_rx.try_recv() {
            message.reject_for_shutdown();
        }
        let mut host_state = self
            .host_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host_state.host = None;
        host_state.active_generation = None;
    }

    fn validate_callback_cell_id(cell_id: &CellId) -> Result<(), String> {
        cell_id
            .validate()
            .map_err(|error| format!("code-mode callback has invalid cell ID: {error}"))
    }

    pub(super) fn cell_is_active(&self, cell_id: &CellId) -> bool {
        self.dispatch_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(cell_id)
    }

    async fn await_callback_cell_admission(
        &self,
        cell_id: &CellId,
        cancellation_token: &CancellationToken,
        cancellation_error: &'static str,
        shutdown_error: &'static str,
    ) -> Result<(), String> {
        if self.dispatcher_shutdown.is_cancelled() {
            return Err(shutdown_error.to_string());
        }
        Self::validate_callback_cell_id(cell_id)?;
        if self.cell_is_active(cell_id) {
            return Ok(());
        }
        let Some(mut resolution_rx) = self.cell_admissions.subscribe_callback(cell_id)? else {
            return self
                .cell_is_active(cell_id)
                .then_some(())
                .ok_or_else(|| "code-mode callback referenced an unknown cell".to_string());
        };

        loop {
            if self.dispatcher_shutdown.is_cancelled() {
                return Err(shutdown_error.to_string());
            }
            match *resolution_rx.borrow_and_update() {
                CellAdmissionResolution::Pending => {}
                CellAdmissionResolution::Admitted => {
                    return self
                        .cell_is_active(cell_id)
                        .then_some(())
                        .ok_or_else(|| "code-mode callback referenced a closed cell".to_string());
                }
                CellAdmissionResolution::Rejected => {
                    return Err("code-mode callback referenced an unknown cell".to_string());
                }
            }
            tokio::select! {
                changed = resolution_rx.changed() => {
                    if changed.is_err() {
                        return Err("code-mode cell admission ended unexpectedly".to_string());
                    }
                }
                _ = cancellation_token.cancelled() => {
                    return Err(cancellation_error.to_string());
                }
                _ = self.dispatcher_shutdown.cancelled() => {
                    return Err(shutdown_error.to_string());
                }
            }
        }
    }
}

fn dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
) -> Option<watch::Sender<bool>> {
    let dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates.get(cell_id).map(|gate| &gate.ready).cloned()
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

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &Mutex<HashMap<CellId, CellDispatchGate>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let Some(ready) = dispatch_gate(dispatch_gates, cell_id) else {
        return false;
    };
    let mut ready_rx = ready.subscribe();
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
            self.await_callback_cell_admission(
                &invocation.cell_id,
                &cancellation_token,
                "code mode nested tool call cancelled",
                INVOKE_DISPATCH_SHUTDOWN_ERROR,
            )
            .await?;
            if self.dispatcher_shutdown.is_cancelled() {
                return Err(INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string());
            }
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let span = tracing::info_span!(
                "code_mode.broker.invoke_tool",
                cell.id = %invocation.cell_id,
                runtime_tool_call_id = invocation.runtime_tool_call_id.as_str(),
                tool_name = invocation.tool_name.name.as_str(),
                tool_namespace = invocation.tool_name.namespace.as_deref(),
            );
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                    span,
                })
                .await
                .map_err(|_| {
                    if self.dispatcher_shutdown.is_cancelled() {
                        INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string()
                    } else {
                        "code mode nested tool dispatcher is unavailable".to_string()
                    }
                })?;
            // Once admitted, the dispatcher owns cancellation and lifecycle cleanup. Await its
            // response so shutdown cannot finish this callback before the nested tool records a
            // terminal outcome.
            response_rx
                .await
                .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?
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
            self.await_callback_cell_admission(
                &cell_id,
                &cancellation_token,
                "code mode notification cancelled",
                NOTIFY_DISPATCH_SHUTDOWN_ERROR,
            )
            .await?;
            if self.dispatcher_shutdown.is_cancelled() {
                return Err(NOTIFY_DISPATCH_SHUTDOWN_ERROR.to_string());
            }
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
                .map_err(|_| {
                    if self.dispatcher_shutdown.is_cancelled() {
                        NOTIFY_DISPATCH_SHUTDOWN_ERROR.to_string()
                    } else {
                        "code mode notification dispatcher is unavailable".to_string()
                    }
                })?;
            tokio::select! {
                biased;
                _ = self.dispatcher_shutdown.cancelled() => {
                    Err(NOTIFY_DISPATCH_SHUTDOWN_ERROR.to_string())
                }
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        if let Err(error) = Self::validate_callback_cell_id(cell_id) {
            tracing::warn!(%error, "ignored invalid code-mode cell-closed callback");
            return;
        }
        if self.cell_is_active(cell_id) {
            self.close_cell(cell_id);
            return;
        }
        match self.cell_admissions.buffer_cell_closed(cell_id) {
            Ok(true) => {}
            Ok(false) if self.cell_is_active(cell_id) => self.close_cell(cell_id),
            Ok(false) => {
                tracing::warn!("ignored code-mode cell-closed callback for an unknown cell");
            }
            Err(error) => {
                tracing::warn!(%error, "ignored invalid code-mode cell-closed callback");
            }
        }
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

impl DispatchMessage {
    fn reject_for_shutdown(self) {
        match self {
            Self::InvokeTool { response_tx, .. } => {
                let _ = response_tx.send(Err(INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string()));
            }
            Self::Notify { response_tx, .. } => {
                let _ = response_tx.send(Err(NOTIFY_DISPATCH_SHUTDOWN_ERROR.to_string()));
            }
        }
    }
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
    #[cfg(test)]
    nested_tool_submissions: Arc<AtomicU64>,
}

impl CoreTurnHost {
    fn submit_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<JsonValue, String>> + Send + 'static {
        #[cfg(test)]
        self.nested_tool_submissions.fetch_add(1, Ordering::SeqCst);
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
        let Some(batch) = self
            .exec
            .session
            .services
            .code_mode_service
            .admit_cell_notification(
                self.exec.session.as_ref(),
                self.exec.turn.as_ref(),
                &cell_id,
                call_id,
                text,
            )
        else {
            return Ok(());
        };
        let result = self
            .exec
            .session
            .inject_if_running_for_turn(&self.exec.turn.sub_id, batch.response_items())
            .await;
        if result.is_err() {
            self.exec
                .session
                .services
                .code_mode_service
                .defer_admitted_cell_inputs(batch);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
