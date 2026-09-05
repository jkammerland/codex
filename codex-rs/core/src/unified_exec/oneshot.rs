//! One-shot execution over the shared unified-exec lifecycle.
//!
//! The caller never receives a resumable process. Timeout and cancellation
//! terminate the exact process handle published by the normal startup path.

use std::sync::Arc;
use std::sync::OnceLock;

use tokio::time::Duration;
use tokio::time::Instant;

use super::ExecCommandRequest;
use super::UnifiedExecContext;
use super::UnifiedExecError;
use super::UnifiedExecProcess;
use super::UnifiedExecProcessManager;
use crate::tools::context::ExecCommandToolOutput;

pub(super) fn request_cancelled_execution_cleanup(process_id_lease: &super::ProcessIdLease) {
    process_id_lease.pending_launch().request_cleanup();
}

pub(super) struct Completion<'a> {
    pub timeout: Duration,
    pub timed_out: bool,
    pub process: &'a OnceLock<Arc<UnifiedExecProcess>>,
}

impl UnifiedExecProcessManager {
    pub(crate) async fn exec_command_to_completion(
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
        timeout: Duration,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let context = UnifiedExecContext::new(
            Arc::clone(&context.session),
            Arc::clone(&context.step_context),
            context.cancellation_token.child_token(),
            context.call_id.clone(),
        );
        // The caller owns this guard. Dropping the one-shot future must cancel
        // the manager-owned child even when the parent turn token remains live
        // and the child has not published a process yet.
        let _cancel_on_origin_drop = context.cancellation_token.clone().drop_guard();
        let pending_launch = Arc::clone(request.process_id_lease.pending_launch());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        pending_launch.spawn_manager_task(async move {
            let manager = &context.session.services.unified_exec_manager;
            let process_id_lease = request.process_id_lease.clone();
            if Instant::now().checked_add(timeout).is_none() {
                manager.release_process_id(&process_id_lease).await;
                let _ = result_tx.send(Err(UnifiedExecError::process_failed(
                    "timeout_ms is too large".into(),
                )));
                return;
            }

            let process = OnceLock::new();
            let mut completion = Completion {
                timeout,
                timed_out: false,
                process: &process,
            };
            let result = {
                let mut execution =
                    Box::pin(manager.exec_command_inner(request, &context, Some(&mut completion)));
                tokio::select! {
                    biased;
                    _ = context.cancellation_token.cancelled() => {
                        if let Some(process) = process.get() {
                            if !process.has_exited()
                                && let Err(err) = process.terminate_confirmed().await
                            {
                                process.fail_and_terminate(err.to_string());
                            }
                            let _ = execution.await;
                        } else {
                            drop(execution);
                            request_cancelled_execution_cleanup(&process_id_lease);
                        }
                        Err(UnifiedExecError::process_failed("command cancelled".into()))
                    }
                    result = &mut execution => result,
                }
            };

            let result = result.map(|mut output| {
                if completion.timed_out {
                    output.process_id = None;
                }
                output
            });
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            UnifiedExecError::process_failed(
                "manager-owned one-shot execution ended without a result".to_string(),
            )
        })?
    }
}
