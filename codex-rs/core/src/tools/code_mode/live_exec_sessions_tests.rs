use codex_code_mode::CellId;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::EXEC_COMMAND_TOOL;
use super::LiveExecSessionRegistry;
use super::MAX_UNIFIED_EXEC_PROCESSES;
use super::WAIT_PROCESS_TOOL;
use super::WRITE_STDIN_TOOL;

#[test]
fn tracks_only_default_namespace_exec_sessions_and_removes_completed_waits() {
    let registry = LiveExecSessionRegistry::default();
    let first_cell = CellId::new("first".to_string());
    let second_cell = CellId::new("second".to_string());

    registry.observe_tool_result(
        &first_cell,
        EXEC_COMMAND_TOOL,
        /*is_default_namespace*/ false,
        /*input_session_id*/ None,
        &json!({"session_id": 10}),
    );
    registry.observe_tool_result(
        &first_cell,
        EXEC_COMMAND_TOOL,
        /*is_default_namespace*/ true,
        /*input_session_id*/ None,
        &json!({"session_id": 11}),
    );
    registry.observe_tool_result(
        &second_cell,
        WRITE_STDIN_TOOL,
        /*is_default_namespace*/ true,
        /*input_session_id*/ Some(11),
        &json!({"session_id": 11}),
    );

    assert_eq!(registry.sessions_for_cell(&first_cell), Vec::<i32>::new());
    assert_eq!(registry.sessions_for_cell(&second_cell), vec![11]);

    registry.observe_tool_result(
        &second_cell,
        WAIT_PROCESS_TOOL,
        /*is_default_namespace*/ true,
        /*input_session_id*/ Some(11),
        &json!({"exit_code": 0}),
    );
    assert_eq!(registry.sessions_for_cell(&second_cell), Vec::<i32>::new());
}

#[test]
fn evicts_the_oldest_observation_at_the_unified_exec_process_limit() {
    let registry = LiveExecSessionRegistry::default();
    let cell_id = CellId::new("cell".to_string());

    for session_id in 0..=MAX_UNIFIED_EXEC_PROCESSES {
        registry.observe_tool_result(
            &cell_id,
            EXEC_COMMAND_TOOL,
            /*is_default_namespace*/ true,
            /*input_session_id*/ None,
            &json!({"session_id": session_id}),
        );
    }

    assert_eq!(
        registry.sessions_for_cell(&cell_id),
        (1..=MAX_UNIFIED_EXEC_PROCESSES)
            .map(|session_id| i32::try_from(session_id).expect("session ID should fit in i32"))
            .collect::<Vec<_>>()
    );
}
