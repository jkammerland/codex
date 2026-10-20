//! Observes a fixed set of accepted assignments without sampling for partial results.

use super::*;
use crate::agent::api::AgentControl;
use crate::session::InputQueueActivity;
use codex_protocol::ThreadId;
use codex_thread_store::AgentTaskFilter;
use codex_thread_store::AgentTaskRecord;
use codex_thread_store::AgentTaskSnapshot;
use codex_thread_store::AgentTaskStatus;
use codex_tools::ToolSpec;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[path = "wait_all_spec.rs"]
mod spec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_all")
    }

    fn spec(&self) -> ToolSpec {
        spec::create_wait_all_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(handle_call(invocation))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

async fn handle_call(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: WaitAllArgs = parse_arguments(&arguments)?;
    let (task_ids, deadline) = args.validate()?;
    let control = &session.services.agent_control;
    if !turn
        .config
        .features
        .enabled(codex_features::Feature::WakeOnAgentMessages)
        || !control.supports_task_barriers()
    {
        return Err(FunctionCallError::RespondToModel(
            "wait_all is unavailable for this agent backend".to_string(),
        ));
    }

    // Subscribe before reading either durable tasks or pending turn input.
    let mut work_updates = control.watch_agent_work();
    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
        .await;
    let (mut activity_updates, _) = session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    let filter = AgentTaskFilter {
        task_ids: task_ids.clone(),
        ..Default::default()
    };
    let initial_tasks = match observe_tasks(control.tasks(filter.clone()), deadline).await {
        Ok(snapshot) => snapshot.tasks,
        Err(reason) => {
            return Ok(boxed_tool_output(WaitAllResult {
                reason,
                all_done: false,
                tasks: Vec::new(),
            }));
        }
    };
    validate_tasks(
        &task_ids,
        &initial_tasks,
        session.thread_id,
        turn.session_source
            .get_agent_path()
            .as_ref()
            .map(codex_protocol::AgentPath::as_str),
    )?;
    control.set_task_barrier(session.thread_id, Some(task_ids.clone()));
    let _barrier = BarrierGuard {
        control: Arc::clone(control),
        thread_id: session.thread_id,
    };
    let started = CollabAgentToolCallItem {
        id: call_id,
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::InProgress,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: initial_tasks.iter().map(|task| task.target_id).collect(),
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: Default::default(),
    };
    session
        .emit_turn_item_started(&turn, &TurnItem::CollabAgentToolCall(started.clone()))
        .await;

    let mut tasks = initial_tasks;
    let reason = loop {
        if let Some(reason) = state_outcome(&tasks) {
            break reason;
        }
        if let Some(activity) = session
            .input_queue
            .pending_barrier_activity(turn_state.as_deref(), &task_ids)
            .await
        {
            break match activity {
                InputQueueActivity::Mailbox => WaitAllReason::Message,
                InputQueueActivity::Steer => WaitAllReason::UserInput,
            };
        }

        let changed = tokio::select! {
            changed = work_updates.changed() => changed.is_ok(),
            changed = activity_updates.changed() => changed.is_ok(),
            () = wait_for_deadline(deadline) => break WaitAllReason::Timeout,
        };
        if !changed {
            break WaitAllReason::Unavailable;
        }
        match observe_tasks(control.tasks(filter.clone()), deadline).await {
            Ok(snapshot) => tasks = snapshot.tasks,
            Err(reason) => break reason,
        }
        // Losing durable observations is not evidence that the assignment failed.
        if tasks.len() != task_ids.len() {
            break WaitAllReason::Unavailable;
        }
    };
    let all_done =
        tasks.len() == task_ids.len() && tasks.iter().all(|task| task.status.is_terminal());
    let result = WaitAllResult {
        reason,
        all_done,
        tasks: task_ids
            .iter()
            .filter_map(|id| tasks.iter().find(|task| task.task_id == *id))
            .map(WaitAllTask::from)
            .collect(),
    };
    session
        .emit_turn_item_completed(
            &turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                status: CollabAgentToolCallStatus::Completed,
                ..started
            }),
        )
        .await;
    Ok(boxed_tool_output(result))
}

struct BarrierGuard {
    control: Arc<dyn AgentControl>,
    thread_id: ThreadId,
}

impl Drop for BarrierGuard {
    fn drop(&mut self) {
        self.control.set_task_barrier(self.thread_id, None);
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitAllArgs {
    task_ids: Vec<String>,
    timeout_ms: Option<i64>,
}

impl WaitAllArgs {
    fn validate(self) -> Result<(Vec<String>, Option<Instant>), FunctionCallError> {
        if self.task_ids.is_empty() || self.task_ids.iter().any(|id| id.trim().is_empty()) {
            return Err(FunctionCallError::RespondToModel(
                "task_ids must contain at least one nonempty accepted assignment ID".to_string(),
            ));
        }
        let deadline = match self.timeout_ms {
            Some(ms) if ms <= 0 => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be positive; omit it to wait without a deadline".to_string(),
                ));
            }
            Some(ms) => Some(
                Instant::now()
                    .checked_add(Duration::from_millis(ms as u64))
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel("timeout_ms is too large".to_string())
                    })?,
            ),
            None => None,
        };
        let mut seen = HashSet::new();
        let task_ids = self
            .task_ids
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();
        Ok((task_ids, deadline))
    }
}

fn validate_tasks(
    task_ids: &[String],
    tasks: &[AgentTaskRecord],
    caller: ThreadId,
    caller_path: Option<&str>,
) -> Result<(), FunctionCallError> {
    for id in task_ids {
        let task = tasks
            .iter()
            .find(|task| task.task_id == *id)
            .ok_or_else(|| FunctionCallError::RespondToModel(format!("unknown task_id: {id}")))?;
        if task.owner_id != caller {
            return Err(FunctionCallError::RespondToModel(format!(
                "task_id {id} belongs to another agent"
            )));
        }
        let ancestor = caller_path
            .zip(task.name.as_deref())
            .is_some_and(|(caller, target)| {
                caller == target
                    || caller
                        .strip_prefix(target)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            });
        if task.target_id == caller || ancestor {
            return Err(FunctionCallError::RespondToModel(format!(
                "task_id {id} depends on this agent or an ancestor"
            )));
        }
    }
    Ok(())
}

fn state_outcome(tasks: &[AgentTaskRecord]) -> Option<WaitAllReason> {
    if !tasks.is_empty() && tasks.iter().all(|task| task.status.is_terminal()) {
        Some(WaitAllReason::AllDone)
    } else if tasks
        .iter()
        .any(|task| task.status == AgentTaskStatus::Held)
    {
        Some(WaitAllReason::Held)
    } else if tasks
        .iter()
        .any(|task| task.status == AgentTaskStatus::CapacityUnavailable)
    {
        Some(WaitAllReason::CapacityUnavailable)
    } else if tasks
        .iter()
        .any(|task| task.status == AgentTaskStatus::Unavailable)
    {
        Some(WaitAllReason::Unavailable)
    } else {
        None
    }
}

// Every store read shares the original deadline; slow observation never extends it.
async fn observe_tasks(
    read: impl std::future::Future<Output = codex_protocol::error::Result<AgentTaskSnapshot>>,
    deadline: Option<Instant>,
) -> Result<AgentTaskSnapshot, WaitAllReason> {
    let result = if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(WaitAllReason::Timeout);
        }
        let result = tokio::time::timeout_at(deadline, read)
            .await
            .map_err(|_| WaitAllReason::Timeout)?;
        if Instant::now() >= deadline {
            return Err(WaitAllReason::Timeout);
        }
        result
    } else {
        read.await
    };
    result.map_err(|_| WaitAllReason::Unavailable)
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WaitAllReason {
    AllDone,
    Message,
    UserInput,
    Timeout,
    Held,
    CapacityUnavailable,
    Unavailable,
}

#[derive(Debug, Serialize)]
struct WaitAllResult {
    reason: WaitAllReason,
    all_done: bool,
    tasks: Vec<WaitAllTask>,
}

#[derive(Debug, Serialize)]
struct WaitAllTask {
    task_id: String,
    task_name: Option<String>,
    status: AgentTaskStatus,
    turn_id: Option<String>,
    result_ref: Option<String>,
}

impl From<&AgentTaskRecord> for WaitAllTask {
    fn from(task: &AgentTaskRecord) -> Self {
        Self {
            task_id: task.task_id.clone(),
            task_name: task.name.clone(),
            status: task.status,
            turn_id: task.turn_id.clone(),
            result_ref: task.result_ref.clone(),
        }
    }
}

impl ToolOutput for WaitAllResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_all")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "wait_all")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_all")
    }
}

#[cfg(test)]
#[path = "wait_all_tests.rs"]
mod tests;
