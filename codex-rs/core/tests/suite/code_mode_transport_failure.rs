use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use codex_features::Feature;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolName;
use core_test_support::TestTargetOs;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

const CELL_ID: &str = "transport-failure-cell";
const OBSERVATION_FAILURE_CELL_ID: &str = "observation-failure-cell";
const CALLBACK_ACCEPTED: &str = "callback accepted after observation failure";

#[derive(Clone, Copy)]
enum FailurePoint {
    InitialResponse,
    Wait,
    Terminate,
}

struct TransportFailureProvider {
    failure_point: FailurePoint,
    command: String,
}

impl codex_code_mode::CodeModeSessionProvider for TransportFailureProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        let session = TransportFailureSession {
            delegate,
            failure_point: self.failure_point,
            command: self.command.clone(),
        };
        Box::pin(async move { Ok(Arc::new(session) as Arc<dyn codex_code_mode::CodeModeSession>) })
    }
}

struct TransportFailureSession {
    delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    failure_point: FailurePoint,
    command: String,
}

#[derive(Clone, Copy)]
enum ObservationFailurePoint {
    InitialResponse,
    Wait,
    ClosedWait,
    ProtocolWait,
    StaleWait,
}

#[derive(Default)]
struct ObservationFailureState {
    delegate: Mutex<Option<Arc<dyn codex_code_mode::CodeModeSessionDelegate>>>,
    terminate_calls: AtomicUsize,
}

struct ObservationFailureProvider {
    failure_point: ObservationFailurePoint,
    callback_command: String,
    state: Arc<ObservationFailureState>,
}

impl codex_code_mode::CodeModeSessionProvider for ObservationFailureProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        self.state
            .delegate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(Arc::clone(&delegate));
        let session = ObservationFailureSession {
            delegate,
            failure_point: self.failure_point,
            callback_command: self.callback_command.clone(),
            state: Arc::clone(&self.state),
        };
        Box::pin(async move { Ok(Arc::new(session) as Arc<dyn codex_code_mode::CodeModeSession>) })
    }
}

struct ObservationFailureSession {
    delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    failure_point: ObservationFailurePoint,
    callback_command: String,
    state: Arc<ObservationFailureState>,
}

impl codex_code_mode::CodeModeSession for ObservationFailureSession {
    fn execute<'a>(
        &'a self,
        _request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let failure_point = self.failure_point;
        Box::pin(async move {
            let cell_id = codex_code_mode::CellId::new(OBSERVATION_FAILURE_CELL_ID.to_string());
            let response_cell_id = cell_id.clone();
            Ok(codex_code_mode::StartedCell::from_future(
                cell_id,
                async move {
                    match failure_point {
                        ObservationFailurePoint::InitialResponse => {
                            Err("injected initial observation failure".to_string())
                        }
                        ObservationFailurePoint::Wait
                        | ObservationFailurePoint::ClosedWait
                        | ObservationFailurePoint::ProtocolWait
                        | ObservationFailurePoint::StaleWait => {
                            Ok(codex_code_mode::RuntimeResponse::Yielded {
                                cell_id: response_cell_id,
                                content_items: Vec::new(),
                                reason: codex_code_mode::YieldReason::Requested,
                                code_mode_host_duration: None,
                            })
                        }
                    }
                },
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: codex_code_mode::WaitRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            match self.failure_point {
                ObservationFailurePoint::Wait => Err("already has an active observer".to_string()),
                ObservationFailurePoint::ProtocolWait => {
                    Ok(codex_code_mode::WaitOutcome::LiveCell(
                        codex_code_mode::RuntimeResponse::Yielded {
                            cell_id: codex_code_mode::CellId::new(
                                "wrong-protocol-cell".to_string(),
                            ),
                            content_items: Vec::new(),
                            reason: codex_code_mode::YieldReason::Requested,
                            code_mode_host_duration: None,
                        },
                    ))
                }
                ObservationFailurePoint::StaleWait => {
                    Ok(codex_code_mode::WaitOutcome::MissingCell(
                        codex_code_mode::RuntimeResponse::Result {
                            cell_id: codex_code_mode::CellId::new(
                                OBSERVATION_FAILURE_CELL_ID.to_string(),
                            ),
                            content_items: Vec::new(),
                            error_text: Some(
                                "cell belongs to a stale code-mode host generation".to_string(),
                            ),
                            code_mode_host_duration: None,
                        },
                    ))
                }
                ObservationFailurePoint::ClosedWait => {
                    self.delegate.cell_closed(&codex_code_mode::CellId::new(
                        OBSERVATION_FAILURE_CELL_ID.to_string(),
                    ));
                    Err("cell closed during observation".to_string())
                }
                ObservationFailurePoint::InitialResponse => {
                    Err("unexpected wait request".to_string())
                }
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        cell_id: codex_code_mode::CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        self.state.terminate_calls.fetch_add(1, Ordering::SeqCst);
        let callback_command = self.callback_command.clone();
        let delegate = Arc::clone(&self.delegate);
        let failure_point = self.failure_point;
        Box::pin(async move {
            let callback_result = delegate
                .invoke_tool(
                    nested_call(
                        cell_id.clone(),
                        "exec_command",
                        serde_json::json!({
                            "cmd": callback_command,
                            "yield_time_ms": 1_000,
                        }),
                    ),
                    CancellationToken::new(),
                )
                .await
                .unwrap_or_else(Value::String);
            let response = codex_code_mode::RuntimeResponse::Terminated {
                cell_id,
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: callback_result.to_string(),
                }],
                code_mode_host_duration: None,
            };
            Ok(match failure_point {
                ObservationFailurePoint::InitialResponse => {
                    codex_code_mode::WaitOutcome::LiveCell(response)
                }
                ObservationFailurePoint::Wait => {
                    codex_code_mode::WaitOutcome::MissingCell(response)
                }
                ObservationFailurePoint::ProtocolWait => {
                    codex_code_mode::WaitOutcome::MissingCell(response)
                }
                ObservationFailurePoint::StaleWait => {
                    codex_code_mode::WaitOutcome::MissingCell(response)
                }
                ObservationFailurePoint::ClosedWait => {
                    codex_code_mode::WaitOutcome::MissingCell(response)
                }
            })
        })
    }

    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl codex_code_mode::CodeModeSession for TransportFailureSession {
    fn execute<'a>(
        &'a self,
        request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let delegate = Arc::clone(&self.delegate);
        let failure_point = self.failure_point;
        let command = self.command.clone();
        Box::pin(async move {
            let cell_id = if request.source.starts_with("recover-session:") {
                codex_code_mode::CellId::new(format!("{CELL_ID}-{}", request.tool_call_id))
            } else {
                codex_code_mode::CellId::new(CELL_ID.to_string())
            };
            let response_cell_id = cell_id.clone();
            let initial_response = async move {
                if let Some(session_id) = request.source.strip_prefix("recover-session:") {
                    let session_id = session_id
                        .parse::<i32>()
                        .map_err(|error| format!("invalid recovery session ID: {error}"))?;
                    let result = delegate
                        .invoke_tool(
                            nested_call(
                                response_cell_id.clone(),
                                "wait_process",
                                serde_json::json!({ "session_id": session_id }),
                            ),
                            CancellationToken::new(),
                        )
                        .await
                        .unwrap_or_else(Value::String);
                    return Ok(codex_code_mode::RuntimeResponse::Result {
                        cell_id: response_cell_id,
                        content_items: vec![
                            codex_code_mode::FunctionCallOutputContentItem::InputText {
                                text: result.to_string(),
                            },
                        ],
                        error_text: None,
                        code_mode_host_duration: None,
                    });
                }

                let nested_result = delegate
                    .invoke_tool(
                        nested_call(
                            response_cell_id.clone(),
                            "exec_command",
                            serde_json::json!({
                                "cmd": command,
                                "yield_time_ms": 250,
                            }),
                        ),
                        CancellationToken::new(),
                    )
                    .await?;
                if nested_result
                    .get("session_id")
                    .and_then(Value::as_i64)
                    .is_none()
                {
                    return Err(format!(
                        "nested command did not yield a recoverable session: {nested_result}"
                    ));
                }
                if matches!(failure_point, FailurePoint::InitialResponse) {
                    return Err("injected initial-response transport failure".to_string());
                }
                Ok(codex_code_mode::RuntimeResponse::Yielded {
                    cell_id: response_cell_id,
                    content_items: Vec::new(),
                    reason: codex_code_mode::YieldReason::Requested,
                    code_mode_host_duration: None,
                })
            };
            Ok(codex_code_mode::StartedCell::from_future(
                cell_id,
                initial_response,
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        _request: codex_code_mode::WaitRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            match self.failure_point {
                FailurePoint::Wait => Err("injected wait transport failure".to_string()),
                FailurePoint::InitialResponse | FailurePoint::Terminate => {
                    Err("unexpected wait request".to_string())
                }
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        _cell_id: codex_code_mode::CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            match self.failure_point {
                FailurePoint::Terminate => Err("injected terminate transport failure".to_string()),
                FailurePoint::InitialResponse | FailurePoint::Wait => {
                    Err("unexpected terminate request".to_string())
                }
            }
        })
    }

    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

fn nested_call(
    cell_id: codex_code_mode::CellId,
    tool_name: &str,
    input: Value,
) -> codex_code_mode::CodeModeNestedToolCall {
    codex_code_mode::CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id: format!("runtime-{tool_name}"),
        tool_name: ToolName::plain(tool_name),
        tool_kind: codex_code_mode::CodeModeToolKind::Function,
        input: Some(input),
    }
}

async fn start_test(server: &MockServer, failure_point: FailurePoint) -> Result<TestCodex> {
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => {
            "while [ ! -f transport.release ]; do sleep 0.01; done; printf completed"
        }
        TestTargetOs::Windows => {
            "while (!(Test-Path -LiteralPath transport.release)) { Start-Sleep -Milliseconds 10 }; Write-Output completed"
        }
    };
    let provider = Arc::new(TransportFailureProvider {
        failure_point,
        command: command.to_string(),
    });
    test_codex()
        .with_code_mode_session_provider(provider)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        })
        .build_with_auto_env(server)
        .await
}

async fn run_exec_turn(
    server: &MockServer,
    test: &TestCodex,
    index: usize,
    source: &str,
) -> Result<ResponsesRequest> {
    let call_id = format!("exec-{index}");
    responses::mount_sse_once(
        server,
        sse(vec![
            ev_response_created(&format!("response-{index}")),
            ev_custom_tool_call(&call_id, "exec", source),
            ev_completed(&format!("response-{index}")),
        ]),
    )
    .await;
    let followup = responses::mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message(&format!("message-{index}"), "done"),
            ev_completed(&format!("followup-{index}")),
        ]),
    )
    .await;
    test.submit_turn(&format!("run transport test step {index}"))
        .await?;
    Ok(followup.single_request())
}

async fn run_wait_turn(
    server: &MockServer,
    test: &TestCodex,
    index: usize,
    cell_id: &str,
    terminate: bool,
) -> Result<ResponsesRequest> {
    let call_id = format!("wait-{index}");
    responses::mount_sse_once(
        server,
        sse(vec![
            ev_response_created(&format!("response-{index}")),
            ev_function_call(
                &call_id,
                "wait",
                &serde_json::to_string(&serde_json::json!({
                    "cell_id": cell_id,
                    "terminate": terminate,
                }))?,
            ),
            ev_completed(&format!("response-{index}")),
        ]),
    )
    .await;
    let followup = responses::mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message(&format!("message-{index}"), "done"),
            ev_completed(&format!("followup-{index}")),
        ]),
    )
    .await;
    test.submit_turn(&format!("observe transport test step {index}"))
        .await?;
    Ok(followup.single_request())
}

fn output_text(request: &ResponsesRequest, call_id: &str, custom: bool) -> String {
    let item = if custom {
        request.custom_tool_call_output(call_id)
    } else {
        request.function_call_output(call_id)
    };
    match item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        output => panic!("unexpected code-mode output: {output:?}"),
    }
}

fn recovery_session_id(output: &str) -> i32 {
    let metadata = output
        .lines()
        .find(|line| line.starts_with("{\"live_sessions\":"))
        .expect("failure output should retain nested-session metadata");
    serde_json::from_str::<Value>(metadata)
        .expect("live-session metadata should be valid JSON")["live_sessions"][0]["session_id"]
        .as_i64()
        .and_then(|session_id| i32::try_from(session_id).ok())
        .expect("live-session metadata should contain an i32 session ID")
}

async fn assert_transport_failure_recovery(
    failure_point: FailurePoint,
    expected_error: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_test(&server, failure_point).await?;
    let initial = run_exec_turn(&server, &test, 1, "start-failing-cell").await?;
    let failure = match failure_point {
        FailurePoint::InitialResponse => initial,
        FailurePoint::Wait => {
            run_wait_turn(&server, &test, 2, CELL_ID, /*terminate*/ false).await?
        }
        FailurePoint::Terminate => {
            run_wait_turn(&server, &test, 2, CELL_ID, /*terminate*/ true).await?
        }
    };
    let (call_id, custom) = match failure_point {
        FailurePoint::InitialResponse => ("exec-1", true),
        FailurePoint::Wait | FailurePoint::Terminate => ("wait-2", false),
    };
    let failure_output = output_text(&failure, call_id, custom);
    assert!(failure_output.contains(expected_error), "{failure_output}");
    let history = test.codex.conversation_history_snapshot().await;
    let success = history.items().find_map(|item| match item {
        ResponseItem::CustomToolCallOutput {
            call_id: item_call_id,
            output,
            ..
        } if custom && item_call_id == call_id => output.success,
        ResponseItem::FunctionCallOutput {
            call_id: Some(item_call_id),
            output,
            ..
        } if !custom && item_call_id == call_id => output.success,
        _ => None,
    });
    assert_eq!(success, Some(false));
    let session_id = recovery_session_id(&failure_output);

    // Keep the original producer alive until the failed observation has exposed its handle,
    // including on native Windows where exec_command has a longer minimum initial yield.
    test.fs()
        .write_file(
            &test.workspace_path_uri("transport.release")?,
            b"release".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    let recovered =
        run_exec_turn(&server, &test, 3, &format!("recover-session:{session_id}")).await?;
    let recovered_output = output_text(&recovered, "exec-3", true);
    assert!(
        recovered_output.contains("completed") && recovered_output.contains("\"exit_code\":0"),
        "first recovery should return the process result: {recovered_output}"
    );

    let replay = run_exec_turn(&server, &test, 4, &format!("recover-session:{session_id}")).await?;
    let replay_output = output_text(&replay, "exec-4", true);
    assert!(
        replay_output.contains("Unknown process id") && !replay_output.contains("exit_code"),
        "a consumed process result must not replay: {replay_output}"
    );
    assert_eq!(
        recovery_session_id_if_present(&recovered_output),
        None,
        "recovery cell must not keep advertising the consumed session"
    );
    Ok(())
}

fn recovery_session_id_if_present(output: &str) -> Option<i32> {
    output
        .lines()
        .find(|line| line.starts_with("{\"live_sessions\":"))
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        .and_then(|metadata| metadata["live_sessions"][0]["session_id"].as_i64())
        .and_then(|session_id| i32::try_from(session_id).ok())
}

async fn assert_observation_failure_preserves_cell(
    failure_point: ObservationFailurePoint,
    expected_error: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let callback_command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => format!("printf '{CALLBACK_ACCEPTED}'"),
        TestTargetOs::Windows => format!("Write-Output '{CALLBACK_ACCEPTED}'"),
    };
    let state = Arc::new(ObservationFailureState::default());
    let provider = Arc::new(ObservationFailureProvider {
        failure_point,
        callback_command,
        state: Arc::clone(&state),
    });
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_code_mode_session_provider(provider)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        })
        .build_with_auto_env(&server)
        .await?;

    let initial = run_exec_turn(&server, &test, 1, "observation-failure").await?;
    let (failure_output, terminate_index) = match failure_point {
        ObservationFailurePoint::InitialResponse => {
            (output_text(&initial, "exec-1", /*custom*/ true), 2)
        }
        ObservationFailurePoint::Wait => {
            let failure = run_wait_turn(
                &server,
                &test,
                2,
                OBSERVATION_FAILURE_CELL_ID,
                /*terminate*/ false,
            )
            .await?;
            (output_text(&failure, "wait-2", /*custom*/ false), 3)
        }
        ObservationFailurePoint::ClosedWait
        | ObservationFailurePoint::ProtocolWait
        | ObservationFailurePoint::StaleWait => {
            panic!("definitive failures use their dedicated terminalization tests")
        }
    };

    let terminated = run_wait_turn(
        &server,
        &test,
        terminate_index,
        OBSERVATION_FAILURE_CELL_ID,
        /*terminate*/ true,
    )
    .await?;
    let terminate_output = output_text(
        &terminated,
        &format!("wait-{terminate_index}"),
        /*custom*/ false,
    );

    assert_eq!(
        state.terminate_calls.load(Ordering::SeqCst),
        1,
        "an observation error must not terminate a cell before the explicit request"
    );
    assert!(
        terminate_output.contains(CALLBACK_ACCEPTED),
        "the dispatch gate must keep accepting callbacks until the terminal response: {terminate_output}"
    );
    assert!(failure_output.contains(expected_error), "{failure_output}");
    assert!(
        failure_output.contains(&format!(
            "Script running with cell ID {OBSERVATION_FAILURE_CELL_ID}"
        )),
        "an observation error must retain the cell recovery handle: {failure_output}"
    );

    let delegate = state
        .delegate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the provider should retain its session delegate");
    let post_terminal_error = delegate
        .invoke_tool(
            nested_call(
                codex_code_mode::CellId::new(OBSERVATION_FAILURE_CELL_ID.to_string()),
                "exec_command",
                serde_json::json!({ "cmd": "unused" }),
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("the terminal response should close the dispatch gate");
    assert_eq!(
        post_terminal_error,
        "code-mode callback referenced an unknown cell"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_observation_failure_preserves_cell_until_explicit_termination() -> Result<()> {
    assert_observation_failure_preserves_cell(
        ObservationFailurePoint::InitialResponse,
        "injected initial observation failure",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_observer_failure_preserves_cell_until_explicit_termination() -> Result<()> {
    assert_observation_failure_preserves_cell(
        ObservationFailurePoint::Wait,
        "already has an active observer",
    )
    .await
}

async fn assert_definitive_observation_failure_terminalizes_cell(
    failure_point: ObservationFailurePoint,
    expected_error: &str,
    expected_terminate_calls: usize,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let callback_command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => format!("printf '{CALLBACK_ACCEPTED}'"),
        TestTargetOs::Windows => format!("Write-Output '{CALLBACK_ACCEPTED}'"),
    };
    let state = Arc::new(ObservationFailureState::default());
    let provider = Arc::new(ObservationFailureProvider {
        failure_point,
        callback_command,
        state: Arc::clone(&state),
    });
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_code_mode_session_provider(provider)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        })
        .build_with_auto_env(&server)
        .await?;

    run_exec_turn(&server, &test, 1, "definitive-observation-failure").await?;
    let failure = run_wait_turn(
        &server,
        &test,
        2,
        OBSERVATION_FAILURE_CELL_ID,
        /*terminate*/ false,
    )
    .await?;
    let failure_output = output_text(&failure, "wait-2", /*custom*/ false);

    assert!(
        failure_output.contains(expected_error),
        "observation failure should remain visible: {failure_output}"
    );
    assert!(
        failure_output.contains("Script failed")
            && !failure_output.contains("Script running with cell ID"),
        "a definitive observation failure must not advertise a running recovery handle: {failure_output}"
    );
    assert_eq!(
        state.terminate_calls.load(Ordering::SeqCst),
        expected_terminate_calls
    );
    let delegate = state
        .delegate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the provider should retain its session delegate");
    assert_eq!(
        delegate
            .invoke_tool(
                nested_call(
                    codex_code_mode::CellId::new(OBSERVATION_FAILURE_CELL_ID.to_string()),
                    "exec_command",
                    serde_json::json!({ "cmd": "unused" }),
                ),
                CancellationToken::new(),
            )
            .await,
        Err("code-mode callback referenced an unknown cell".to_string())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_observation_failure_terminalizes_cell_with_failed_status() -> Result<()> {
    assert_definitive_observation_failure_terminalizes_cell(
        ObservationFailurePoint::ClosedWait,
        "cell closed during observation",
        /*expected_terminate_calls*/ 0,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_observation_failure_terminalizes_cell_with_failed_status() -> Result<()> {
    assert_definitive_observation_failure_terminalizes_cell(
        ObservationFailurePoint::ProtocolWait,
        "code-mode host returned cell ID wrong-protocol-cell for request targeting observation-failure-cell",
        /*expected_terminate_calls*/ 1,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_outcome_terminalizes_active_gate_with_failed_status() -> Result<()> {
    assert_definitive_observation_failure_terminalizes_cell(
        ObservationFailurePoint::StaleWait,
        "cell belongs to a stale code-mode host generation",
        /*expected_terminate_calls*/ 0,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_response_transport_failure_retains_nested_session_recovery() -> Result<()> {
    assert_transport_failure_recovery(
        FailurePoint::InitialResponse,
        "injected initial-response transport failure",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_transport_failure_retains_nested_session_recovery() -> Result<()> {
    assert_transport_failure_recovery(FailurePoint::Wait, "injected wait transport failure").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_transport_failure_retains_nested_session_recovery() -> Result<()> {
    assert_transport_failure_recovery(
        FailurePoint::Terminate,
        "injected terminate transport failure",
    )
    .await
}
