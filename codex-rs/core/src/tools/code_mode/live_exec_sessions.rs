use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use codex_code_mode::CellId;
use serde_json::Value as JsonValue;

const EXEC_COMMAND_TOOL: &str = "exec_command";
const WAIT_PROCESS_TOOL: &str = "wait_process";
const WRITE_STDIN_TOOL: &str = "write_stdin";

#[derive(Default)]
pub(super) struct LiveExecSessionRegistry {
    state: Mutex<LiveExecSessionState>,
}

#[derive(Default)]
struct LiveExecSessionState {
    sessions_by_cell: HashMap<CellId, BTreeSet<i32>>,
    observation_order: VecDeque<i32>,
}

impl LiveExecSessionRegistry {
    pub(super) fn observe_tool_result(
        &self,
        cell_id: &CellId,
        tool_name: &str,
        is_default_namespace: bool,
        input_session_id: Option<i32>,
        result: &JsonValue,
    ) {
        if !is_default_namespace {
            return;
        }

        let result_session_id = result
            .get("session_id")
            .and_then(JsonValue::as_i64)
            .and_then(|session_id| i32::try_from(session_id).ok());

        match tool_name {
            EXEC_COMMAND_TOOL => {
                if let Some(session_id) = result_session_id {
                    self.move_to_cell(session_id, cell_id);
                }
            }
            WAIT_PROCESS_TOOL | WRITE_STDIN_TOOL => {
                if let Some(session_id) = input_session_id {
                    self.remove(session_id);
                }
                if let Some(session_id) = result_session_id {
                    self.move_to_cell(session_id, cell_id);
                }
            }
            _ => {}
        }
    }

    pub(super) fn sessions_for_cell(&self, cell_id: &CellId) -> Vec<i32> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sessions_by_cell
            .get(cell_id)
            .map(|sessions| sessions.iter().copied().collect())
            .unwrap_or_default()
    }

    fn move_to_cell(&self, session_id: i32, cell_id: &CellId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.remove(session_id);
        state
            .observation_order
            .push_back(session_id);
        state
            .sessions_by_cell
            .entry(cell_id.clone())
            .or_default()
            .insert(session_id);
        while state.observation_order.len() > MAX_UNIFIED_EXEC_PROCESSES {
            if let Some(oldest_session_id) = state.observation_order.pop_front() {
                remove_session_from_cells(&mut state.sessions_by_cell, oldest_session_id);
            }
        }
    }

    fn remove(&self, session_id: i32) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
    }
}

impl LiveExecSessionState {
    fn remove(&mut self, session_id: i32) {
        self.observation_order
            .retain(|observed_session_id| *observed_session_id != session_id);
        remove_session_from_cells(&mut self.sessions_by_cell, session_id);
    }
}

fn remove_session_from_cells(
    sessions_by_cell: &mut HashMap<CellId, BTreeSet<i32>>,
    session_id: i32,
) {
    sessions_by_cell.retain(|_, sessions| {
        sessions.remove(&session_id);
        !sessions.is_empty()
    });
}

#[cfg(test)]
#[path = "live_exec_sessions_tests.rs"]
mod tests;
