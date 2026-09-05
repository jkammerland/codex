use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

use futures::FutureExt;
use futures::poll;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::super::CodeModeService;
use super::super::ExecContext;
use super::CodeModeDispatchBroker;
use super::DispatchMessage;
use super::INVOKE_DISPATCH_SHUTDOWN_ERROR;
use crate::config::CodeModeConfig;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeToolKind;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_protocol::ToolName;
use codex_protocol::openai_models::ToolMode;
use codex_tools::FunctionCallError;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolExecutorFuture;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;

type OwnedToolCallback = Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>;

struct BlockingNestedHandler {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    invocations: Arc<AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for BlockingNestedHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("blocking_nested_tool")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: self.tool_name().name,
            description: "Blocking nested test tool.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, _invocation: ToolInvocation) -> ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Box::pin(async move {
            if let Some(started) = started {
                let _ = started.send(());
            }
            std::future::pending::<Result<Box<dyn ToolOutput>, FunctionCallError>>().await
        })
    }
}

impl CoreToolRuntime for BlockingNestedHandler {}

struct NestedLifecycleObserver {
    starts: Arc<AtomicUsize>,
    outcomes: Arc<Mutex<Vec<ToolCallOutcome>>>,
    finished: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl ToolLifecycleContributor for NestedLifecycleObserver {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        let is_target = input.tool_name.name == "blocking_nested_tool"
            && matches!(
                input.source,
                codex_extension_api::ToolCallSource::CodeMode { .. }
            );
        Box::pin(async move {
            if is_target {
                self.starts.fetch_add(1, Ordering::SeqCst);
            }
        })
    }

    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
        let is_target = input.tool_name.name == "blocking_nested_tool"
            && matches!(
                input.source,
                codex_extension_api::ToolCallSource::CodeMode { .. }
            );
        Box::pin(async move {
            if !is_target {
                return;
            }
            self.outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(input.outcome);
            if let Some(finished) = self
                .finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = finished.send(());
            }
        })
    }
}

struct RacingCallbackSessionProvider {
    pending_callback: Arc<Mutex<Option<OwnedToolCallback>>>,
}

impl codex_code_mode::CodeModeSessionProvider for RacingCallbackSessionProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        let pending_callback = Arc::clone(&self.pending_callback);
        Box::pin(async move {
            Ok(Arc::new(RacingCallbackSession {
                delegate,
                pending_callback,
            }) as Arc<dyn codex_code_mode::CodeModeSession>)
        })
    }
}

struct RacingCallbackSession {
    delegate: Arc<dyn CodeModeSessionDelegate>,
    pending_callback: Arc<Mutex<Option<OwnedToolCallback>>>,
}

struct ShutdownRacingSessionProvider {
    execute_entered: Arc<AtomicBool>,
    release_execute: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    terminated_cells: Arc<Mutex<Vec<CellId>>>,
}

impl codex_code_mode::CodeModeSessionProvider for ShutdownRacingSessionProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        let session = ShutdownRacingSession {
            execute_entered: Arc::clone(&self.execute_entered),
            release_execute: Arc::clone(&self.release_execute),
            terminated_cells: Arc::clone(&self.terminated_cells),
        };
        Box::pin(async move { Ok(Arc::new(session) as Arc<dyn codex_code_mode::CodeModeSession>) })
    }
}

struct ShutdownRacingSession {
    execute_entered: Arc<AtomicBool>,
    release_execute: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    terminated_cells: Arc<Mutex<Vec<CellId>>>,
}

impl codex_code_mode::CodeModeSession for ShutdownRacingSession {
    fn execute<'a>(
        &'a self,
        _request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let execute_entered = Arc::clone(&self.execute_entered);
        let release_execute = self
            .release_execute
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("shutdown race test execute should run once");
        Box::pin(async move {
            execute_entered.store(true, Ordering::SeqCst);
            let _ = release_execute.await;
            let cell_id = CellId::new("shutdown-racing-cell".to_string());
            let response_cell_id = cell_id.clone();
            Ok(codex_code_mode::StartedCell::from_future(
                cell_id,
                async move {
                    Ok(codex_code_mode::RuntimeResponse::Yielded {
                        cell_id: response_cell_id,
                        content_items: Vec::new(),
                        reason: codex_code_mode::YieldReason::Requested,
                        code_mode_host_duration: None,
                    })
                },
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: codex_code_mode::WaitRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async { Err("shutdown race test session cannot wait".to_string()) })
    }

    fn terminate<'a>(
        &'a self,
        cell_id: CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        self.terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(cell_id.clone());
        Box::pin(async move {
            Ok(codex_code_mode::WaitOutcome::MissingCell(
                codex_code_mode::RuntimeResponse::Terminated {
                    cell_id,
                    content_items: Vec::new(),
                    code_mode_host_duration: None,
                },
            ))
        })
    }

    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl codex_code_mode::CodeModeSession for RacingCallbackSession {
    fn execute<'a>(
        &'a self,
        _request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let delegate = Arc::clone(&self.delegate);
        let pending_callback = Arc::clone(&self.pending_callback);
        Box::pin(async move {
            let cell_id = CellId::new("provider-racing-cell".to_string());
            let callback_cell_id = cell_id.clone();
            let mut callback: OwnedToolCallback = Box::pin(async move {
                delegate
                    .invoke_tool(nested_call(callback_cell_id), CancellationToken::new())
                    .await
            });
            assert_eq!(
                poll!(callback.as_mut()),
                Poll::Pending,
                "provider callback should wait for StartedCell admission"
            );
            pending_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace(callback);
            let response_cell_id = cell_id.clone();
            Ok(codex_code_mode::StartedCell::from_future(
                cell_id,
                async move {
                    Ok(codex_code_mode::RuntimeResponse::Yielded {
                        cell_id: response_cell_id,
                        content_items: Vec::new(),
                        reason: codex_code_mode::YieldReason::Requested,
                        code_mode_host_duration: None,
                    })
                },
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: codex_code_mode::WaitRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async { Err("racing test session cannot wait".to_string()) })
    }

    fn terminate<'a>(
        &'a self,
        _cell_id: CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async { Err("racing test session cannot terminate".to_string()) })
    }

    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

fn broker_with_known_cell() -> (CodeModeDispatchBroker, CellId) {
    let broker = CodeModeDispatchBroker::new(None);
    let known_cell_id = CellId::new("known-cell".to_string());
    let admission = broker
        .begin_cell_admission()
        .expect("known test admission should begin");
    broker
        .admit_cell(admission, &known_cell_id)
        .expect("known test cell should be admitted");
    (broker, known_cell_id)
}

fn nested_call(cell_id: CellId) -> CodeModeNestedToolCall {
    CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id: "runtime-call".to_string(),
        tool_name: ToolName::plain("read_file"),
        tool_kind: CodeModeToolKind::Function,
        input: None,
    }
}

fn blocking_nested_call(cell_id: CellId) -> CodeModeNestedToolCall {
    CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id: "blocking-runtime-call".to_string(),
        tool_name: ToolName::plain("blocking_nested_tool"),
        tool_kind: CodeModeToolKind::Function,
        input: None,
    }
}

#[test]
fn invalid_and_unknown_delegate_callbacks_fail_without_entering_the_broker_queue() {
    let invalid_cell_ids = [
        CellId::new(String::new()),
        CellId::new("🦀".repeat(/*n*/ 2_501)),
        CellId::new("different-cell".to_string()),
    ];

    for invalid_cell_id in invalid_cell_ids {
        let (broker, known_cell_id) = broker_with_known_cell();
        let expected_error = invalid_cell_id.validate().map_or_else(
            |error| format!("code-mode callback has invalid cell ID: {error}"),
            |()| "code-mode callback referenced an unknown cell".to_string(),
        );

        let invocation_error = broker
            .invoke_tool(
                nested_call(invalid_cell_id.clone()),
                CancellationToken::new(),
            )
            .now_or_never()
            .expect("invalid nested invocation should resolve synchronously")
            .expect_err("invalid nested invocation should fail");
        assert_eq!(invocation_error, expected_error);

        let notification_error = broker
            .notify(
                "runtime-call".to_string(),
                invalid_cell_id.clone(),
                "notification".to_string(),
                CancellationToken::new(),
            )
            .now_or_never()
            .expect("invalid notification should resolve synchronously")
            .expect_err("invalid notification should fail");
        assert_eq!(notification_error, expected_error);

        broker.cell_closed(&invalid_cell_id);
        assert_eq!(broker.active_cell_ids(), vec![known_cell_id]);
        assert_eq!(broker.dispatch_rx.len(), 0);
        if invalid_cell_id.as_str().len() > codex_code_mode::MAX_CELL_ID_BYTES {
            assert!(!expected_error.contains(invalid_cell_id.as_str()));
        }
    }
}

#[test]
fn callbacks_started_after_shutdown_fail_without_entering_the_broker_queue() {
    let (broker, cell_id) = broker_with_known_cell();
    broker.shutdown();

    let invoke_error = broker
        .invoke_tool(nested_call(cell_id.clone()), CancellationToken::new())
        .now_or_never()
        .expect("post-shutdown invocation should resolve synchronously")
        .expect_err("post-shutdown invocation should fail");
    let notify_error = broker
        .notify(
            "runtime-call".to_string(),
            cell_id,
            "notification".to_string(),
            CancellationToken::new(),
        )
        .now_or_never()
        .expect("post-shutdown notification should resolve synchronously")
        .expect_err("post-shutdown notification should fail");

    assert_eq!(
        invoke_error,
        "code mode nested tool dispatcher is shut down"
    );
    assert_eq!(
        notify_error,
        "code mode notification dispatcher is shut down"
    );
    assert_eq!(broker.dispatch_rx.len(), 0);
}

#[tokio::test]
async fn shutdown_releases_queued_callbacks_and_preserves_dequeued_invoke_ownership() {
    let (broker, cell_id) = broker_with_known_cell();
    let mut inflight_invoke =
        broker.invoke_tool(nested_call(cell_id.clone()), CancellationToken::new());
    assert_eq!(poll!(inflight_invoke.as_mut()), Poll::Pending);
    let held_invoke = broker
        .dispatch_rx
        .try_recv()
        .expect("invocation should enter the dispatch queue");
    assert!(matches!(&held_invoke, DispatchMessage::InvokeTool { .. }));

    let mut inflight_notify = broker.notify(
        "inflight-notify".to_string(),
        cell_id.clone(),
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(inflight_notify.as_mut()), Poll::Pending);
    let held_notify = broker
        .dispatch_rx
        .try_recv()
        .expect("notification should enter the dispatch queue");
    assert!(matches!(&held_notify, DispatchMessage::Notify { .. }));

    let mut queued_invoke =
        broker.invoke_tool(nested_call(cell_id.clone()), CancellationToken::new());
    let mut queued_notify = broker.notify(
        "queued-notify".to_string(),
        cell_id,
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(queued_invoke.as_mut()), Poll::Pending);
    assert_eq!(poll!(queued_notify.as_mut()), Poll::Pending);
    assert_eq!(broker.dispatch_rx.len(), 2);

    broker.shutdown();

    let invoke_error = |result| match result {
        Poll::Ready(Err(error)) => error,
        Poll::Ready(Ok(_)) => panic!("callback unexpectedly succeeded during shutdown"),
        Poll::Pending => panic!("callback remained pending after shutdown"),
    };
    assert_eq!(
        invoke_error(poll!(queued_invoke.as_mut())),
        "code mode nested tool dispatcher is shut down"
    );
    let notify_error = |result| match result {
        Poll::Ready(Err(error)) => error,
        Poll::Ready(Ok(())) => panic!("notification unexpectedly succeeded during shutdown"),
        Poll::Pending => panic!("notification remained pending after shutdown"),
    };
    assert_eq!(
        notify_error(poll!(queued_notify.as_mut())),
        "code mode notification dispatcher is shut down"
    );
    assert_eq!(broker.dispatch_rx.len(), 0);

    assert!(poll!(inflight_invoke.as_mut()).is_pending());
    assert_eq!(
        notify_error(poll!(inflight_notify.as_mut())),
        "code mode notification dispatcher is shut down"
    );

    held_invoke.reject_for_shutdown();
    held_notify.reject_for_shutdown();

    assert_eq!(
        invoke_error(poll!(inflight_invoke.as_mut())),
        "code mode nested tool dispatcher is shut down"
    );
}

#[tokio::test]
async fn provider_callback_before_execute_returns_is_released_to_dispatch() {
    let pending_callback = Arc::new(Mutex::new(None));
    let service = CodeModeService::new(
        Arc::new(RacingCallbackSessionProvider {
            pending_callback: Arc::clone(&pending_callback),
        }),
        &CodeModeConfig::default(),
        /*executed_tool_calls*/ None,
    );
    let started = service
        .execute(codex_code_mode::ExecuteRequest {
            tool_call_id: "provider-race".to_string(),
            enabled_tools: Vec::new(),
            source: "await tools.read_file({});".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .expect("provider should return its StartedCell");
    assert_eq!(
        started.cell_id,
        CellId::new("provider-racing-cell".to_string())
    );
    assert_eq!(service.dispatch_broker.dispatch_rx.len(), 0);

    let mut callback = pending_callback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("provider should retain the pre-admission callback");
    assert_eq!(poll!(callback.as_mut()), Poll::Pending);
    assert_eq!(service.dispatch_broker.dispatch_rx.len(), 1);
}

#[tokio::test]
async fn mismatched_and_abandoned_pre_admission_callbacks_fail_on_lifecycle_signal() {
    let broker = CodeModeDispatchBroker::new(None);
    let admission = broker
        .begin_cell_admission()
        .expect("cell admission should begin");
    let mut mismatched = broker.notify(
        "runtime-call".to_string(),
        CellId::new("different-cell".to_string()),
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(mismatched.as_mut()), Poll::Pending);

    let admitted_cell_id = CellId::new("admitted-cell".to_string());
    broker
        .admit_cell(admission, &admitted_cell_id)
        .expect("provider's started cell should complete admission");
    assert_eq!(
        poll!(mismatched.as_mut()),
        Poll::Ready(Err(
            "code-mode callback referenced an unknown cell".to_string()
        ))
    );
    assert_eq!(broker.dispatch_rx.len(), 0);

    let abandoned_admission = broker
        .begin_cell_admission()
        .expect("second cell admission should begin");
    let mut abandoned = broker.notify(
        "runtime-call".to_string(),
        CellId::new("never-admitted-cell".to_string()),
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(abandoned.as_mut()), Poll::Pending);
    drop(abandoned_admission);
    assert_eq!(
        poll!(abandoned.as_mut()),
        Poll::Ready(Err(
            "code-mode callback referenced an unknown cell".to_string()
        ))
    );
    assert_eq!(broker.dispatch_rx.len(), 0);
}

#[tokio::test]
async fn pre_admission_callbacks_bind_only_to_attempts_active_when_observed() {
    let broker = CodeModeDispatchBroker::new(None);
    let first_admission = broker
        .begin_cell_admission()
        .expect("first cell admission should begin");
    let target_cell_id = CellId::new("target-cell".to_string());
    let mut early_callback = broker.notify(
        "early-call".to_string(),
        target_cell_id.clone(),
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(early_callback.as_mut()), Poll::Pending);

    let later_admission = broker
        .begin_cell_admission()
        .expect("later cell admission should begin");
    let mut later_callback = broker.notify(
        "later-call".to_string(),
        target_cell_id,
        "notification".to_string(),
        CancellationToken::new(),
    );
    assert_eq!(poll!(later_callback.as_mut()), Poll::Pending);

    broker
        .admit_cell(first_admission, &CellId::new("other-cell".to_string()))
        .expect("first provider cell should complete admission");
    assert_eq!(
        poll!(early_callback.as_mut()),
        Poll::Ready(Err(
            "code-mode callback referenced an unknown cell".to_string()
        ))
    );
    assert_eq!(poll!(later_callback.as_mut()), Poll::Pending);

    drop(later_admission);
    assert_eq!(
        poll!(later_callback.as_mut()),
        Poll::Ready(Err(
            "code-mode callback referenced an unknown cell".to_string()
        ))
    );
    assert_eq!(broker.dispatch_rx.len(), 0);
}

#[tokio::test]
async fn shutdown_rejects_inflight_execute_without_resurrecting_its_cell() {
    let execute_entered = Arc::new(AtomicBool::new(false));
    let (release_execute_tx, release_execute_rx) = tokio::sync::oneshot::channel();
    let terminated_cells = Arc::new(Mutex::new(Vec::new()));
    let service = CodeModeService::new(
        Arc::new(ShutdownRacingSessionProvider {
            execute_entered: Arc::clone(&execute_entered),
            release_execute: Arc::new(Mutex::new(Some(release_execute_rx))),
            terminated_cells: Arc::clone(&terminated_cells),
        }),
        &CodeModeConfig::default(),
        None,
    );
    let mut execute = Box::pin(service.execute(codex_code_mode::ExecuteRequest {
        tool_call_id: "shutdown-race".to_string(),
        enabled_tools: Vec::new(),
        source: "yield_control();".to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    }));

    assert!(matches!(poll!(execute.as_mut()), Poll::Pending));
    assert!(execute_entered.load(Ordering::SeqCst));
    service.shutdown().await.expect("service should shut down");
    release_execute_tx
        .send(())
        .expect("execute future should still await its release signal");
    let error = match poll!(execute.as_mut()) {
        Poll::Ready(Err(error)) => error,
        Poll::Ready(Ok(_)) => panic!("cell returned after shutdown must be rejected"),
        Poll::Pending => panic!("released execute should resolve synchronously"),
    };

    assert_eq!(error, "code mode session is shutting down");
    assert_eq!(
        service.dispatch_broker.active_cell_ids(),
        Vec::<CellId>::new()
    );
    assert_eq!(service.dispatch_broker.dispatch_rx.len(), 0);
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![CellId::new("shutdown-racing-cell".to_string())]
    );
    let begin_error = match service.dispatch_broker.begin_cell_admission() {
        Ok(_) => panic!("shutdown broker must reject new admissions"),
        Err(error) => error,
    };
    assert_eq!(begin_error, "code mode session is shutting down");
}

#[tokio::test]
async fn pre_admission_callback_buffer_rejects_over_capacity_without_dispatching() {
    let broker = CodeModeDispatchBroker::new(None);
    let admission = broker
        .begin_cell_admission()
        .expect("cell admission should begin");
    let cell_id = CellId::new("buffered-cell".to_string());
    let mut callbacks = Vec::new();
    for index in 0..256 {
        let mut callback = broker.notify(
            format!("runtime-call-{index}"),
            cell_id.clone(),
            "notification".to_string(),
            CancellationToken::new(),
        );
        assert_eq!(poll!(callback.as_mut()), Poll::Pending);
        callbacks.push(callback);
    }

    let overflow_error = broker
        .notify(
            "overflow".to_string(),
            cell_id,
            "notification".to_string(),
            CancellationToken::new(),
        )
        .now_or_never()
        .expect("overflow callback should resolve synchronously")
        .expect_err("overflow callback should be rejected");
    assert_eq!(
        overflow_error,
        "too many code-mode callbacks are awaiting cell admission"
    );
    assert_eq!(broker.dispatch_rx.len(), 0);

    drop(admission);
    drop(callbacks);
}

#[test]
fn cell_closed_before_started_cell_admission_is_applied_after_admission() {
    let broker = CodeModeDispatchBroker::new(None);
    let admission = broker
        .begin_cell_admission()
        .expect("cell admission should begin");
    let cell_id = CellId::new("closed-racing-cell".to_string());

    broker.cell_closed(&cell_id);
    broker
        .admit_cell(admission, &cell_id)
        .expect("provider's started cell should complete admission");

    assert_eq!(broker.active_cell_ids(), Vec::<CellId>::new());
    assert_eq!(broker.dispatch_rx.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_between_worker_check_and_host_install_does_not_install_a_host() {
    let broker = Arc::new(CodeModeDispatchBroker::new(None));
    let install_barrier = Arc::new(std::sync::Barrier::new(2));
    broker
        .host_install_barrier
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(Arc::clone(&install_barrier));
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let exec = ExecContext {
        session,
        turn: Arc::clone(&turn),
    };
    let step_context = StepContext::for_test(turn);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let worker_broker = Arc::clone(&broker);
    let worker_task = tokio::task::spawn_blocking(move || {
        worker_broker.start_turn_worker(exec, step_context, tracker)
    });

    let entered_barrier = Arc::clone(&install_barrier);
    tokio::task::spawn_blocking(move || entered_barrier.wait())
        .await
        .expect("worker should reach the host-install barrier");
    broker.shutdown();
    tokio::task::spawn_blocking(move || install_barrier.wait())
        .await
        .expect("worker should leave the host-install barrier");
    let worker = worker_task.await.expect("worker should finish installing");

    let host_state = broker
        .host_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(host_state.host.is_none());
    assert_eq!(host_state.active_generation, None);
    drop(host_state);
    drop(worker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_after_dequeue_rejects_before_nested_tool_submission() {
    let broker = Arc::new(CodeModeDispatchBroker::new(None));
    let submission_barrier = Arc::new(tokio::sync::Barrier::new(2));
    broker
        .nested_tool_submission_barrier
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(Arc::clone(&submission_barrier));
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let exec = ExecContext {
        session,
        turn: Arc::clone(&turn),
    };
    let step_context = StepContext::for_test(turn);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let worker = broker.start_turn_worker(exec, step_context, tracker);
    let cell_id = CellId::new("shutdown-before-submit".to_string());
    let admission = broker
        .begin_cell_admission()
        .expect("test cell admission should begin");
    broker
        .admit_cell(admission, &cell_id)
        .expect("test cell should be admitted");
    broker.mark_cell_ready_for_dispatch(&cell_id, /*originating_item_id*/ None);

    let invoking_broker = Arc::clone(&broker);
    let invocation_cell_id = cell_id.clone();
    let invocation = tokio::spawn(async move {
        invoking_broker
            .invoke_tool(nested_call(invocation_cell_id), CancellationToken::new())
            .await
    });
    submission_barrier.wait().await;
    broker.shutdown();
    submission_barrier.wait().await;

    assert_eq!(
        invocation.await.expect("callback task should finish"),
        Err("code mode nested tool dispatcher is shut down".to_string())
    );
    assert_eq!(broker.nested_tool_submissions.load(Ordering::SeqCst), 0);
    drop(worker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_pre_ready_callback_does_not_close_cell_dispatch() {
    let (_release_execute_tx, release_execute_rx) = tokio::sync::oneshot::channel();
    let terminated_cells = Arc::new(Mutex::new(Vec::new()));
    let service = CodeModeService::new(
        Arc::new(ShutdownRacingSessionProvider {
            execute_entered: Arc::new(AtomicBool::new(false)),
            release_execute: Arc::new(Mutex::new(Some(release_execute_rx))),
            terminated_cells: Arc::clone(&terminated_cells),
        }),
        &CodeModeConfig::default(),
        /*executed_tool_calls*/ None,
    );
    service
        .session()
        .await
        .expect("interrupt test session should initialize");
    let broker = Arc::clone(&service.dispatch_broker);
    let readiness_barrier = Arc::new(tokio::sync::Barrier::new(2));
    broker
        .readiness_wait_barrier
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(Arc::clone(&readiness_barrier));
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let worker = broker.start_turn_worker(
        ExecContext {
            session,
            turn: Arc::clone(&turn),
        },
        step_context,
        tracker,
    );
    let cell_id = CellId::new("cancelled-pre-ready-callback".to_string());
    let admission = broker
        .begin_cell_admission()
        .expect("test cell admission should begin");
    broker
        .admit_cell(admission, &cell_id)
        .expect("test cell should be admitted");

    let cancellation = CancellationToken::new();
    let invoking_broker = Arc::clone(&broker);
    let invocation_cell_id = cell_id.clone();
    let invocation_cancellation = cancellation.clone();
    let first_callback = tokio::spawn(async move {
        invoking_broker
            .invoke_tool(nested_call(invocation_cell_id), invocation_cancellation)
            .await
    });
    readiness_barrier.wait().await;
    cancellation.cancel();
    readiness_barrier.wait().await;
    assert_eq!(
        first_callback
            .await
            .expect("cancelled callback task should finish"),
        Err("code mode nested tool call cancelled".to_string())
    );

    assert_eq!(
        broker.active_cell_ids(),
        vec![cell_id.clone()],
        "interrupt discovery must retain the cell after one callback is cancelled"
    );
    broker.mark_cell_ready_for_dispatch(&cell_id, /*originating_item_id*/ None);
    broker
        .notify(
            "second-callback".to_string(),
            cell_id.clone(),
            String::new(),
            CancellationToken::new(),
        )
        .await
        .expect("a later callback for the same live cell should succeed");
    assert_eq!(broker.active_cell_ids(), vec![cell_id]);
    service.interrupt_active_cells().await;
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![CellId::new("cancelled-pre-ready-callback".to_string())]
    );
    drop(worker);
}

#[tokio::test]
async fn shutdown_after_nested_handler_start_emits_one_aborted_outcome_without_replay() {
    let (handler_started_tx, handler_started_rx) = tokio::sync::oneshot::channel();
    let invocations = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(BlockingNestedHandler {
        started: Mutex::new(Some(handler_started_tx)),
        invocations: Arc::clone(&invocations),
    }) as Arc<dyn CoreToolRuntime>;

    let starts = Arc::new(AtomicUsize::new(0));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let (lifecycle_finished_tx, lifecycle_finished_rx) = tokio::sync::oneshot::channel();
    let mut extensions = ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.tool_lifecycle_contributor(Arc::new(NestedLifecycleObserver {
        starts: Arc::clone(&starts),
        outcomes: Arc::clone(&outcomes),
        finished: Mutex::new(Some(lifecycle_finished_tx)),
    }));

    let (mut session, turn) = make_session_and_context().await;
    session.services.extensions = Arc::new(extensions.build());
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step_context = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
        ToolMode::CodeMode,
        BTreeMap::new(),
        /*tool_namespaces_info*/ None,
        &[],
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let broker = Arc::new(CodeModeDispatchBroker::new(None));
    let worker = broker.start_turn_worker(
        ExecContext {
            session,
            turn: Arc::clone(&turn),
        },
        step_context,
        tracker,
    );
    let cell_id = CellId::new("shutdown-after-handler-start".to_string());
    let admission = broker
        .begin_cell_admission()
        .expect("test cell admission should begin");
    broker
        .admit_cell(admission, &cell_id)
        .expect("test cell should be admitted");
    broker.mark_cell_ready_for_dispatch(&cell_id, None);

    let invoking_broker = Arc::clone(&broker);
    let invocation = tokio::spawn(async move {
        invoking_broker
            .invoke_tool(blocking_nested_call(cell_id), CancellationToken::new())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), handler_started_rx)
        .await
        .expect("nested handler should start before shutdown")
        .expect("nested handler start signal should be delivered");

    broker.shutdown();

    assert_eq!(
        invocation.await.expect("callback task should finish"),
        Err(INVOKE_DISPATCH_SHUTDOWN_ERROR.to_string())
    );
    lifecycle_finished_rx
        .now_or_never()
        .expect("callback completion must wait for the nested terminal lifecycle outcome")
        .expect("nested lifecycle finish signal should be delivered");
    tokio::task::yield_now().await;

    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        *outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![ToolCallOutcome::Aborted]
    );
    drop(worker);
}
