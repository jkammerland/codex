use std::sync::Arc;
use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::CodeModeService;
use crate::config::CodeModeConfig;

struct StaticCellSessionProvider {
    started_cell_id: codex_code_mode::CellId,
    response_cell_id: codex_code_mode::CellId,
    terminate_response_cell_id: codex_code_mode::CellId,
    initial_response_error: Option<String>,
    missing_wait_response: bool,
    terminated_cells: Arc<Mutex<Vec<codex_code_mode::CellId>>>,
}

impl codex_code_mode::CodeModeSessionProvider for StaticCellSessionProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        let started_cell_id = self.started_cell_id.clone();
        let response_cell_id = self.response_cell_id.clone();
        let terminate_response_cell_id = self.terminate_response_cell_id.clone();
        let initial_response_error = self.initial_response_error.clone();
        let missing_wait_response = self.missing_wait_response;
        let terminated_cells = Arc::clone(&self.terminated_cells);
        Box::pin(async move {
            Ok(Arc::new(StaticCellSession {
                started_cell_id,
                response_cell_id,
                terminate_response_cell_id,
                initial_response_error,
                missing_wait_response,
                terminated_cells,
            }) as Arc<dyn codex_code_mode::CodeModeSession>)
        })
    }
}

struct StaticCellSession {
    started_cell_id: codex_code_mode::CellId,
    response_cell_id: codex_code_mode::CellId,
    terminate_response_cell_id: codex_code_mode::CellId,
    initial_response_error: Option<String>,
    missing_wait_response: bool,
    terminated_cells: Arc<Mutex<Vec<codex_code_mode::CellId>>>,
}

impl codex_code_mode::CodeModeSession for StaticCellSession {
    fn execute<'a>(
        &'a self,
        _request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let started_cell_id = self.started_cell_id.clone();
        let response_cell_id = self.response_cell_id.clone();
        let initial_response_error = self.initial_response_error.clone();
        Box::pin(async move {
            Ok(codex_code_mode::StartedCell::from_future(
                started_cell_id,
                async move {
                    if let Some(error) = initial_response_error {
                        return Err(error);
                    }
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
        let response_cell_id = self.response_cell_id.clone();
        let missing_wait_response = self.missing_wait_response;
        Box::pin(async move {
            let response = codex_code_mode::RuntimeResponse::Yielded {
                cell_id: response_cell_id,
                content_items: Vec::new(),
                reason: codex_code_mode::YieldReason::Requested,
                code_mode_host_duration: None,
            };
            Ok(if missing_wait_response {
                codex_code_mode::WaitOutcome::MissingCell(response)
            } else {
                codex_code_mode::WaitOutcome::LiveCell(response)
            })
        })
    }

    fn terminate<'a>(
        &'a self,
        cell_id: codex_code_mode::CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        self.terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(cell_id);
        let response_cell_id = self.terminate_response_cell_id.clone();
        Box::pin(async move {
            Ok(codex_code_mode::WaitOutcome::MissingCell(
                codex_code_mode::RuntimeResponse::Terminated {
                    cell_id: response_cell_id,
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

#[tokio::test]
async fn custom_session_provider_rejects_oversized_cell_ids_for_tiny_output_budgets() {
    let oversized = "🦀".repeat(/*n*/ 2_501);
    for max_output_tokens in [0, 1] {
        let terminated_cells = Arc::new(Mutex::new(Vec::new()));
        let service = CodeModeService::new(
            Arc::new(StaticCellSessionProvider {
                started_cell_id: codex_code_mode::CellId::new(oversized.clone()),
                response_cell_id: codex_code_mode::CellId::new(oversized.clone()),
                terminate_response_cell_id: codex_code_mode::CellId::new(oversized.clone()),
                initial_response_error: None,
                missing_wait_response: false,
                terminated_cells: Arc::clone(&terminated_cells),
            }),
            &CodeModeConfig::default(),
            None,
        );
        let error = service
            .execute(codex_code_mode::ExecuteRequest {
                tool_call_id: "oversized-cell".to_string(),
                enabled_tools: Vec::new(),
                source: "yield_control();".to_string(),
                yield_time_ms: None,
                max_output_tokens: Some(max_output_tokens),
            })
            .await
            .err()
            .expect("custom session provider should reject oversized cell ID");

        assert_eq!(
            error,
            format!(
                "code-mode host returned invalid cell ID: cell ID is {} bytes; maximum is {} bytes",
                oversized.len(),
                codex_code_mode::MAX_CELL_ID_BYTES
            )
        );
        assert!(!error.contains(&oversized));
        assert_eq!(
            *terminated_cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![codex_code_mode::CellId::new(oversized.clone())]
        );
    }
}

fn static_service(
    started_cell_id: &str,
    response_cell_id: &str,
    initial_response_error: Option<&str>,
) -> (CodeModeService, Arc<Mutex<Vec<codex_code_mode::CellId>>>) {
    static_service_with_missing_wait(
        started_cell_id,
        response_cell_id,
        initial_response_error,
        false,
    )
}

fn static_service_with_missing_wait(
    started_cell_id: &str,
    response_cell_id: &str,
    initial_response_error: Option<&str>,
    missing_wait_response: bool,
) -> (CodeModeService, Arc<Mutex<Vec<codex_code_mode::CellId>>>) {
    static_service_with_responses(
        started_cell_id,
        response_cell_id,
        response_cell_id,
        initial_response_error,
        missing_wait_response,
    )
}

fn static_service_with_responses(
    started_cell_id: &str,
    response_cell_id: &str,
    terminate_response_cell_id: &str,
    initial_response_error: Option<&str>,
    missing_wait_response: bool,
) -> (CodeModeService, Arc<Mutex<Vec<codex_code_mode::CellId>>>) {
    let terminated_cells = Arc::new(Mutex::new(Vec::new()));
    let service = CodeModeService::new(
        Arc::new(StaticCellSessionProvider {
            started_cell_id: codex_code_mode::CellId::new(started_cell_id.to_string()),
            response_cell_id: codex_code_mode::CellId::new(response_cell_id.to_string()),
            terminate_response_cell_id: codex_code_mode::CellId::new(
                terminate_response_cell_id.to_string(),
            ),
            initial_response_error: initial_response_error.map(str::to_string),
            missing_wait_response,
            terminated_cells: Arc::clone(&terminated_cells),
        }),
        &CodeModeConfig::default(),
        None,
    );
    (service, terminated_cells)
}

fn execute_request() -> codex_code_mode::ExecuteRequest {
    codex_code_mode::ExecuteRequest {
        tool_call_id: "mismatched-cell".to_string(),
        enabled_tools: Vec::new(),
        source: "yield_control();".to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn custom_session_provider_rejects_mismatched_execute_response_cell_id() {
    let (service, terminated_cells) = static_service("requested-cell", "different-cell", None);
    let started = service
        .execute(execute_request())
        .await
        .expect("started cell ID is valid");

    let error = started
        .initial_response()
        .await
        .expect_err("initial response must refer to the started cell");
    assert_eq!(
        error,
        "code-mode host returned cell ID different-cell for request targeting requested-cell"
    );
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![codex_code_mode::CellId::new("requested-cell".to_string())]
    );
}

#[tokio::test]
async fn invalid_initial_response_terminates_and_closes_the_started_cell() {
    let (service, terminated_cells) = static_service("requested-cell", "", None);
    let started = service
        .execute(execute_request())
        .await
        .expect("started cell ID should be admitted");

    assert_eq!(
        started
            .initial_response()
            .await
            .expect_err("invalid initial response must fail"),
        "code-mode host returned invalid cell ID: cell ID must not be empty"
    );
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![codex_code_mode::CellId::new("requested-cell".to_string())]
    );
    assert_eq!(
        service.dispatch_broker.active_cell_ids(),
        Vec::<codex_code_mode::CellId>::new()
    );
}

#[tokio::test]
async fn failed_initial_observation_preserves_the_started_cell_for_recovery() {
    let (service, terminated_cells) = static_service(
        "requested-cell",
        "requested-cell",
        Some("provider initial response failed"),
    );
    let started = service
        .execute(execute_request())
        .await
        .expect("started cell ID should be admitted");

    assert_eq!(
        started
            .initial_response()
            .await
            .expect_err("failed initial observation must fail"),
        "provider initial response failed"
    );
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Vec::<codex_code_mode::CellId>::new()
    );
    assert_eq!(
        service.dispatch_broker.active_cell_ids(),
        vec![codex_code_mode::CellId::new("requested-cell".to_string())]
    );
}

#[tokio::test]
async fn custom_session_provider_rejects_mismatched_wait_response_cell_id() {
    let (service, terminated_cells) = static_service("requested-cell", "different-cell", None);
    let error = service
        .wait(codex_code_mode::WaitRequest {
            cell_id: codex_code_mode::CellId::new("requested-cell".to_string()),
            yield_time_ms: 1,
        })
        .await
        .expect_err("wait response must refer to the requested cell");

    assert_eq!(
        error,
        "code-mode host returned cell ID different-cell for request targeting requested-cell"
    );
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![codex_code_mode::CellId::new("requested-cell".to_string())]
    );
}

#[tokio::test]
async fn missing_cell_yielded_response_is_rejected_and_terminalizes_the_cell() {
    let (service, terminated_cells) = static_service_with_missing_wait(
        "requested-cell",
        "requested-cell",
        /*initial_response_error*/ None,
        /*missing_wait_response*/ true,
    );
    let started = service
        .execute(execute_request())
        .await
        .expect("test cell should be admitted");
    started
        .initial_response()
        .await
        .expect("initial response should keep the test cell live");

    assert_eq!(
        service
            .wait(codex_code_mode::WaitRequest {
                cell_id: codex_code_mode::CellId::new("requested-cell".to_string()),
                yield_time_ms: 1,
            })
            .await,
        Err("code-mode host returned a yielded response for a missing cell".to_string())
    );
    assert_eq!(
        *terminated_cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![codex_code_mode::CellId::new("requested-cell".to_string())]
    );
    assert_eq!(
        service.dispatch_broker.active_cell_ids(),
        Vec::<codex_code_mode::CellId>::new()
    );
}

#[tokio::test]
async fn custom_session_provider_rejects_mismatched_terminate_response_cell_id() {
    let (service, _) = static_service_with_responses(
        "requested-cell",
        "requested-cell",
        "different-cell",
        /*initial_response_error*/ None,
        /*missing_wait_response*/ false,
    );
    let started = service
        .execute(execute_request())
        .await
        .expect("test cell should be admitted");
    started
        .initial_response()
        .await
        .expect("initial response should keep the test cell live");
    let error = service
        .terminate(codex_code_mode::CellId::new("requested-cell".to_string()))
        .await
        .expect_err("terminate response must refer to the requested cell");

    assert_eq!(
        error,
        "code-mode host returned cell ID different-cell for request targeting requested-cell"
    );
    assert_eq!(
        service.dispatch_broker.active_cell_ids(),
        Vec::<codex_code_mode::CellId>::new()
    );
}
