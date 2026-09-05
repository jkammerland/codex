use codex_code_mode::CellId;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use codex_tools::ToolName;

use super::PUBLIC_TOOL_NAME;
use super::WAIT_TOOL_NAME;

pub(super) fn validate_core_tool_call_id(
    tool_name: &ToolName,
    call_id: &str,
) -> Result<(), String> {
    let is_code_mode_tool = tool_name.is_default_namespace()
        && matches!(tool_name.name.as_str(), PUBLIC_TOOL_NAME | WAIT_TOOL_NAME);
    if !is_code_mode_tool {
        return Ok(());
    }
    if call_id.is_empty() {
        return Err("code-mode tool call ID must not be empty".to_string());
    }
    if call_id.len() > codex_code_mode::grpc::MAX_IDENTIFIER_BYTES {
        return Err(format!(
            "code-mode tool call ID is {} bytes; maximum is {} bytes",
            call_id.len(),
            codex_code_mode::grpc::MAX_IDENTIFIER_BYTES,
        ));
    }
    Ok(())
}

pub(super) fn validate_host_cell_id(cell_id: &CellId) -> Result<(), String> {
    cell_id
        .validate()
        .map_err(|error| format!("code-mode host returned invalid cell ID: {error}"))
}

pub(super) fn validate_host_runtime_response(response: &RuntimeResponse) -> Result<(), String> {
    validate_host_cell_id(runtime_response_cell_id(response))
}

pub(super) fn validate_runtime_response_for_cell(
    expected_cell_id: &CellId,
    response: &RuntimeResponse,
) -> Result<(), String> {
    validate_host_runtime_response(response)?;
    let response_cell_id = runtime_response_cell_id(response);
    if response_cell_id != expected_cell_id {
        return Err(format!(
            "code-mode host returned cell ID {response_cell_id} for request targeting {expected_cell_id}"
        ));
    }
    Ok(())
}

pub(super) fn validate_wait_outcome_for_cell(
    expected_cell_id: &CellId,
    outcome: &WaitOutcome,
) -> Result<(), String> {
    match outcome {
        WaitOutcome::LiveCell(response) => {
            validate_runtime_response_for_cell(expected_cell_id, response)
        }
        WaitOutcome::MissingCell(response) => {
            validate_runtime_response_for_cell(expected_cell_id, response)?;
            if matches!(response, RuntimeResponse::Yielded { .. }) {
                return Err(
                    "code-mode host returned a yielded response for a missing cell".to_string(),
                );
            }
            Ok(())
        }
    }
}

fn runtime_response_cell_id(response: &RuntimeResponse) -> &CellId {
    match response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}
