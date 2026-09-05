use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::GrpcCodeModeSessionProvider;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode_protocol::grpc;
use codex_code_mode_protocol::grpc::code_mode_host_server::CodeModeHost;
use codex_code_mode_protocol::grpc::code_mode_host_server::CodeModeHostServer;
use futures::Stream;
use futures::StreamExt;
use futures::stream;
use pretty_assertions::assert_eq;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::Response;
use tonic::Status;
use tonic::transport::Server;

type GrpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;
type GrpcFuture<'a, T> = Pin<Box<dyn Future<Output = Result<Response<T>, Status>> + Send + 'a>>;

#[derive(Clone, Copy)]
enum ObservationMode {
    Healthy,
    InitialTransportFailure,
    WaitTransportFailure,
    TerminateTransportFailure,
    MalformedInitial,
    MalformedWait,
    MalformedTerminate,
}

struct ObservationHost {
    mode: ObservationMode,
    termination_requests: mpsc::UnboundedSender<grpc::TerminateRequest>,
}

struct RecordingDelegate {
    closed_cells: mpsc::UnboundedSender<CellId>,
}

impl CodeModeSessionDelegate for RecordingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Err("nested tools are not used".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let _ = self.closed_cells.send(cell_id.clone());
    }
}

impl CodeModeHost for ObservationHost {
    type OpenSessionStream = GrpcStream<grpc::SessionEvent>;
    type SubscribeToToolCallsStream = GrpcStream<grpc::ToolCall>;
    type ExecuteStream = GrpcStream<grpc::ExecuteEvent>;

    fn open_session<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::OpenSessionRequest>,
    ) -> GrpcFuture<'async_trait, Self::OpenSessionStream>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async {
            let opened = grpc::SessionEvent {
                event: Some(grpc::session_event::Event::Opened(grpc::SessionOpened {
                    session_id: "session-1".to_string(),
                })),
            };
            let events = stream::once(async { Ok(opened) }).chain(stream::pending());
            Ok(Response::new(Box::pin(events) as Self::OpenSessionStream))
        })
    }

    fn close_session<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::CloseSessionRequest>,
    ) -> GrpcFuture<'async_trait, grpc::CloseSessionResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(Response::new(grpc::CloseSessionResponse {})) })
    }

    fn subscribe_to_tool_calls<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::SubscribeToToolCallsRequest>,
    ) -> GrpcFuture<'async_trait, Self::SubscribeToToolCallsStream>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async {
            Ok(Response::new(
                Box::pin(stream::pending()) as Self::SubscribeToToolCallsStream
            ))
        })
    }

    fn complete_tool_call<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::CompleteToolCallRequest>,
    ) -> GrpcFuture<'async_trait, grpc::CompleteToolCallResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Err(Status::unimplemented("tool callbacks are not used")) })
    }

    fn acknowledge_notification<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::AcknowledgeNotificationRequest>,
    ) -> GrpcFuture<'async_trait, grpc::AcknowledgeNotificationResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Err(Status::unimplemented("notifications are not used")) })
    }

    fn execute<'a, 'async_trait>(
        &'a self,
        request: Request<grpc::ExecuteRequest>,
    ) -> GrpcFuture<'async_trait, Self::ExecuteStream>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let request = request.into_inner();
            let started = grpc::ExecuteEvent {
                event: Some(grpc::execute_event::Event::Started(
                    grpc::ExecutionStarted {
                        execution_id: request.execution_id,
                        cell_id: "cell-1".to_string(),
                    },
                )),
            };
            let outcome = match self.mode {
                ObservationMode::InitialTransportFailure => Err(Status::unavailable(
                    "injected initial outcome transport failure",
                )),
                ObservationMode::MalformedInitial => Ok(grpc::ExecuteEvent { event: None }),
                ObservationMode::Healthy
                | ObservationMode::WaitTransportFailure
                | ObservationMode::TerminateTransportFailure
                | ObservationMode::MalformedWait
                | ObservationMode::MalformedTerminate => Ok(grpc::ExecuteEvent {
                    event: Some(grpc::execute_event::Event::Outcome(yielded_outcome(
                        "cell-1",
                    ))),
                }),
            };
            let events = stream::iter([Ok(started), outcome]);
            Ok(Response::new(Box::pin(events) as Self::ExecuteStream))
        })
    }

    fn wait<'a, 'async_trait>(
        &'a self,
        request: Request<grpc::WaitRequest>,
    ) -> GrpcFuture<'async_trait, grpc::WaitResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let request = request.into_inner();
            if matches!(self.mode, ObservationMode::WaitTransportFailure) {
                return Err(Status::unavailable("injected wait transport failure"));
            }
            if matches!(self.mode, ObservationMode::MalformedWait) {
                return Ok(Response::new(grpc::WaitResponse { state: None }));
            }
            Ok(Response::new(grpc::WaitResponse {
                state: Some(grpc::wait_response::State::LiveCell(yielded_outcome(
                    &request.cell_id,
                ))),
            }))
        })
    }

    fn cancel_wait<'a, 'async_trait>(
        &'a self,
        _request: Request<grpc::CancelWaitRequest>,
    ) -> GrpcFuture<'async_trait, grpc::CancelWaitResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Err(Status::unimplemented("wait cancellation is not used")) })
    }

    fn terminate<'a, 'async_trait>(
        &'a self,
        request: Request<grpc::TerminateRequest>,
    ) -> GrpcFuture<'async_trait, grpc::WaitResponse>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let request = request.into_inner();
            self.termination_requests
                .send(request.clone())
                .map_err(|_| Status::internal("termination observer is unavailable"))?;
            if matches!(self.mode, ObservationMode::TerminateTransportFailure) {
                return Err(Status::unavailable(
                    "injected termination transport failure",
                ));
            }
            if matches!(self.mode, ObservationMode::MalformedTerminate) {
                return Ok(Response::new(grpc::WaitResponse { state: None }));
            }
            Ok(Response::new(grpc::WaitResponse {
                state: Some(grpc::wait_response::State::MissingCell(
                    grpc::ExecutionOutcome {
                        cell_id: request.cell_id,
                        content_items: Vec::new(),
                        code_mode_host_duration_ns: 0,
                        outcome: Some(grpc::execution_outcome::Outcome::Terminated(
                            grpc::ExecutionTerminated {},
                        )),
                    },
                )),
            }))
        })
    }
}

fn yielded_outcome(cell_id: &str) -> grpc::ExecutionOutcome {
    grpc::ExecutionOutcome {
        cell_id: cell_id.to_string(),
        content_items: Vec::new(),
        code_mode_host_duration_ns: 0,
        outcome: Some(grpc::execution_outcome::Outcome::Yielded(
            grpc::ExecutionYielded {
                reason: grpc::YieldReason::Requested.into(),
            },
        )),
    }
}

fn execute_request() -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "outer-call".to_string(),
        enabled_tools: Vec::new(),
        source: "yield_control();".to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn delivered_initial_outcome_failure_has_one_termination_owner() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake gRPC host");
    let address = listener.local_addr().expect("fake gRPC host address");
    let incoming = stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let (termination_tx, mut termination_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(CodeModeHostServer::new(ObservationHost {
                mode: ObservationMode::InitialTransportFailure,
                termination_requests: termination_tx,
            }))
            .serve_with_incoming(incoming)
            .await
            .expect("serve fake gRPC host");
    });
    let session = GrpcCodeModeSessionProvider::new(format!("http://{address}"))
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("open fake gRPC session");
    let started = session
        .execute(execute_request())
        .await
        .expect("receive started cell");
    let cell_id = started.cell_id.clone();

    let error = started
        .initial_response()
        .await
        .expect_err("initial outcome transport failure should reach the consumer");
    assert!(error.contains("injected initial outcome transport failure"));
    let cleanup = session
        .terminate(cell_id.clone())
        .await
        .expect("consumer should clean up the admitted cell");
    assert!(matches!(cleanup, WaitOutcome::MissingCell(_)));

    let first = timeout(Duration::from_secs(5), termination_rx.recv())
        .await
        .expect("consumer cleanup did not reach the gRPC host")
        .expect("gRPC host dropped its termination observer");
    assert_eq!(first.cell_id, "cell-1");
    assert_ne!(cell_id.as_str(), first.cell_id);
    assert!(
        timeout(Duration::from_millis(250), termination_rx.recv())
            .await
            .is_err(),
        "delivered initial-response failure issued a duplicate termination RPC"
    );

    session.shutdown().await.expect("shut down gRPC session");
    server.abort();
}

#[tokio::test]
async fn malformed_initial_wait_and_terminate_observations_close_the_session() {
    for mode in [
        ObservationMode::MalformedInitial,
        ObservationMode::MalformedWait,
        ObservationMode::MalformedTerminate,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake gRPC host");
        let address = listener.local_addr().expect("fake gRPC host address");
        let incoming = stream::unfold(listener, |listener| async move {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let (termination_tx, _termination_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(CodeModeHostServer::new(ObservationHost {
                    mode,
                    termination_requests: termination_tx,
                }))
                .serve_with_incoming(incoming)
                .await
                .expect("serve fake gRPC host");
        });
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel();
        let session = GrpcCodeModeSessionProvider::new(format!("http://{address}"))
            .create_session(Arc::new(RecordingDelegate {
                closed_cells: closed_tx,
            }))
            .await
            .expect("open fake gRPC session");
        let started = session
            .execute(execute_request())
            .await
            .expect("receive started cell");
        let cell_id = started.cell_id.clone();
        let initial = started.initial_response().await;
        let error = match mode {
            ObservationMode::MalformedInitial => {
                initial.expect_err("malformed initial observation should fail")
            }
            ObservationMode::MalformedWait => {
                initial.expect("valid initial observation before malformed wait");
                session
                    .wait(WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: 1,
                    })
                    .await
                    .expect_err("malformed wait observation should fail")
            }
            ObservationMode::MalformedTerminate => {
                initial.expect("valid initial observation before malformed termination");
                session
                    .terminate(cell_id.clone())
                    .await
                    .expect_err("malformed termination observation should fail")
            }
            ObservationMode::InitialTransportFailure => {
                unreachable!("transport behavior is covered separately")
            }
            ObservationMode::Healthy
            | ObservationMode::WaitTransportFailure
            | ObservationMode::TerminateTransportFailure => {
                unreachable!("transport behavior is covered separately")
            }
        };
        assert!(
            error.contains("unexpected event") || error.contains("omitted its outcome"),
            "unexpected malformed-observation error: {error}"
        );
        assert_eq!(
            timeout(Duration::from_secs(5), closed_rx.recv())
                .await
                .expect("malformed observation did not close its cell"),
            Some(cell_id.clone()),
        );
        let stale_outcome = session
            .wait(WaitRequest {
                cell_id,
                yield_time_ms: 1,
            })
            .await
            .expect("reconnect should classify the closed generation as stale");
        assert!(matches!(
            stale_outcome,
            WaitOutcome::MissingCell(codex_code_mode::RuntimeResponse::Result {
                error_text: Some(ref error),
                ..
            }) if error == "cell belongs to a stale code-mode host generation"
        ));

        let _ = session.shutdown().await;
        server.abort();
    }
}

#[tokio::test]
async fn wait_and_terminate_transport_failures_keep_the_grpc_binding_usable() {
    for mode in [
        ObservationMode::WaitTransportFailure,
        ObservationMode::TerminateTransportFailure,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake gRPC host");
        let address = listener.local_addr().expect("fake gRPC host address");
        let incoming = stream::unfold(listener, |listener| async move {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let (termination_tx, _termination_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(CodeModeHostServer::new(ObservationHost {
                    mode,
                    termination_requests: termination_tx,
                }))
                .serve_with_incoming(incoming)
                .await
                .expect("serve fake gRPC host");
        });
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel();
        let session = GrpcCodeModeSessionProvider::new(format!("http://{address}"))
            .create_session(Arc::new(RecordingDelegate {
                closed_cells: closed_tx,
            }))
            .await
            .expect("open fake gRPC session");
        let started = session
            .execute(execute_request())
            .await
            .expect("receive started cell");
        let cell_id = started.cell_id.clone();
        assert!(matches!(
            started
                .initial_response()
                .await
                .expect("receive yielded initial response"),
            codex_code_mode::RuntimeResponse::Yielded { .. }
        ));

        match mode {
            ObservationMode::WaitTransportFailure => {
                let error = session
                    .wait(WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: 1,
                    })
                    .await
                    .expect_err("wait transport failure should reach the consumer");
                assert!(error.contains("injected wait transport failure"));
                assert!(closed_rx.try_recv().is_err());
                assert!(matches!(
                    session
                        .terminate(cell_id)
                        .await
                        .expect("same binding should remain usable for termination"),
                    WaitOutcome::MissingCell(codex_code_mode::RuntimeResponse::Terminated { .. })
                ));
            }
            ObservationMode::TerminateTransportFailure => {
                let error = session
                    .terminate(cell_id.clone())
                    .await
                    .expect_err("termination transport failure should reach the consumer");
                assert!(error.contains("injected termination transport failure"));
                assert!(closed_rx.try_recv().is_err());
                assert!(matches!(
                    session
                        .wait(WaitRequest {
                            cell_id,
                            yield_time_ms: 1,
                        })
                        .await
                        .expect("same binding should remain usable for another observation"),
                    WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded { .. })
                ));
            }
            ObservationMode::Healthy
            | ObservationMode::InitialTransportFailure
            | ObservationMode::MalformedInitial
            | ObservationMode::MalformedWait
            | ObservationMode::MalformedTerminate => {
                unreachable!("only wait and termination transport failures are exercised")
            }
        }

        session.shutdown().await.expect("shut down gRPC session");
        server.abort();
    }
}

#[tokio::test]
async fn resumed_grpc_session_cannot_alias_a_reused_host_cell_id() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake gRPC host");
    let address = listener.local_addr().expect("fake gRPC host address");
    let incoming = stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let (termination_tx, _termination_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(CodeModeHostServer::new(ObservationHost {
                mode: ObservationMode::Healthy,
                termination_requests: termination_tx,
            }))
            .serve_with_incoming(incoming)
            .await
            .expect("serve fake gRPC host");
    });
    let provider = GrpcCodeModeSessionProvider::new(format!("http://{address}"));
    let previous = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("open previous gRPC session");
    let previous_started = previous
        .execute(execute_request())
        .await
        .expect("start previous cell");
    let previous_handle = previous_started.cell_id.clone();
    assert!(matches!(
        previous_started
            .initial_response()
            .await
            .expect("receive previous initial response"),
        codex_code_mode::RuntimeResponse::Yielded { .. }
    ));
    previous
        .shutdown()
        .await
        .expect("shut down previous gRPC session");

    let resumed = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("open resumed gRPC session");
    let resumed_started = resumed
        .execute(execute_request())
        .await
        .expect("start resumed cell with reused host ID");
    let resumed_handle = resumed_started.cell_id.clone();
    assert!(matches!(
        resumed_started
            .initial_response()
            .await
            .expect("receive resumed initial response"),
        codex_code_mode::RuntimeResponse::Yielded { .. }
    ));

    assert_ne!(previous_handle, resumed_handle);
    assert_ne!(previous_handle.as_str(), "cell-1");
    assert_ne!(resumed_handle.as_str(), "cell-1");
    previous_handle
        .validate()
        .expect("previous public handle should remain bounded");
    resumed_handle
        .validate()
        .expect("resumed public handle should remain bounded");
    assert!(matches!(
        resumed
            .wait(WaitRequest {
                cell_id: previous_handle,
                yield_time_ms: 1,
            })
            .await
            .expect("old handle should become a terminal stale result"),
        WaitOutcome::MissingCell(codex_code_mode::RuntimeResponse::Result {
            error_text: Some(ref error),
            ..
        }) if error == "cell belongs to a stale code-mode host generation"
    ));
    assert!(matches!(
        resumed
            .wait(WaitRequest {
                cell_id: resumed_handle.clone(),
                yield_time_ms: 1,
            })
            .await
            .expect("resumed handle should address the current cell"),
        WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded {
            cell_id,
            ..
        }) if cell_id == resumed_handle
    ));

    resumed
        .shutdown()
        .await
        .expect("shut down resumed gRPC session");
    server.abort();
}
