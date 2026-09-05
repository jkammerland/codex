use std::sync::Arc;
use std::sync::Weak;

use codex_core_plugins::PluginCommandAttribution;
use codex_utils_path_uri::PathUri;
use tokio::time::Instant;

use super::ProcessIdLease;
use super::SharedPluginMetricsSidecar;
use super::async_watcher::emit_failed_exec_end_for_unified_exec;
use super::async_watcher::spawn_exit_watcher;
use super::head_tail_buffer::HeadTailBuffer;
use super::pending_launch::ManagerTaskHandle;
use super::pending_launch::PendingLaunchDecision;
use super::pending_launch::PendingLaunchOutcome;
use super::pending_launch::PendingLaunchTerminalEvent;
use super::process::UnifiedExecProcess;
use super::process_manager::finish_deferred_network_approval_for_session;
use super::take_plugin_metrics_sidecar;
use crate::plugins::metrics::finish_and_track_measurements;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::network_approval::DeferredNetworkApproval;

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct PendingLaunchResources {
    pub(super) process: Arc<UnifiedExecProcess>,
    pub(super) session: Weak<Session>,
    pub(super) turn: Arc<TurnContext>,
    pub(super) call_id: String,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathUri,
    pub(super) process_id: i32,
    pub(super) plugin_attribution: Option<PluginCommandAttribution>,
    pub(super) transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    pub(super) started_at: Instant,
    pub(super) network_approval: Option<DeferredNetworkApproval>,
    pub(super) network_denial_monitor: Option<ManagerTaskHandle>,
    pub(super) plugin_metrics_sidecar: Option<SharedPluginMetricsSidecar>,
}

pub(super) fn spawn_pending_launch_supervisor(
    process_id_lease: ProcessIdLease,
    decision_rx: tokio::sync::oneshot::Receiver<PendingLaunchDecision>,
    resources: PendingLaunchResources,
) {
    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let supervisor_launch = Arc::clone(&pending_launch);
    pending_launch.spawn_manager_task(async move {
        let decision = decision_rx.await.unwrap_or_else(|_| {
            supervisor_launch.suppress_events();
            PendingLaunchDecision::Cleanup {
                terminal_event: PendingLaunchTerminalEvent::Suppress,
            }
        });
        let outcome = match decision {
            PendingLaunchDecision::Cleanup { terminal_event } => {
                cleanup_pending_launch(&process_id_lease, resources, terminal_event).await
            }
            PendingLaunchDecision::WatchProcess => {
                let mut outcome = PendingLaunchOutcome::default();
                if let Some(session) = resources.session.upgrade() {
                    let network_approval = resources.network_approval.clone();
                    let watcher = spawn_exit_watcher(
                        resources.process,
                        Arc::clone(&session),
                        resources.turn,
                        resources.call_id,
                        resources.command,
                        resources.cwd,
                        resources.process_id,
                        resources.plugin_attribution,
                        resources.transcript,
                        resources.started_at,
                        resources.network_denial_monitor,
                        resources.plugin_metrics_sidecar,
                    );
                    let _abort_watcher_on_drop = AbortTaskOnDrop(watcher.abort_handle());
                    let _ = watcher.await;
                    outcome.network_approval_error = finish_deferred_network_approval_for_session(
                        Some(&session),
                        network_approval,
                    )
                    .await
                    .err();
                    session
                        .services
                        .unified_exec_manager
                        .release_process_reservation(&process_id_lease)
                        .await;
                } else if !resources.process.has_exited()
                    && let Err(error) = resources.process.terminate_confirmed().await
                {
                    resources
                        .process
                        .mark_termination_failure(error.to_string());
                }
                outcome
            }
        };
        supervisor_launch.complete(outcome);
    });
}

async fn cleanup_pending_launch(
    process_id_lease: &ProcessIdLease,
    resources: PendingLaunchResources,
    terminal_event: PendingLaunchTerminalEvent,
) -> PendingLaunchOutcome {
    let PendingLaunchResources {
        process,
        session,
        turn,
        call_id,
        command,
        cwd,
        process_id,
        plugin_attribution,
        transcript,
        started_at,
        network_approval,
        network_denial_monitor,
        plugin_metrics_sidecar,
    } = resources;
    let output_drained = (terminal_event == PendingLaunchTerminalEvent::Emit)
        .then(|| process.output_drained_notify());
    if let Some(network_denial_monitor) = network_denial_monitor {
        network_denial_monitor.cancel_and_wait().await;
    }
    let termination_error = if process.has_exited() {
        None
    } else {
        process
            .terminate_confirmed()
            .await
            .err()
            .map(|err| err.to_string())
    };
    if let Some(error) = termination_error.as_ref() {
        process.mark_termination_failure(error.clone());
    }
    let session = session.upgrade();
    let network_approval_error =
        finish_deferred_network_approval_for_session(session.as_ref(), network_approval)
            .await
            .err();
    let metrics_sidecar = plugin_metrics_sidecar
        .as_ref()
        .and_then(take_plugin_metrics_sidecar);
    if let Some(output_drained) = output_drained {
        output_drained.notified().await;
    }
    if let Some(session) = session.as_ref() {
        finish_and_track_measurements(
            metrics_sidecar,
            process.exit_code().unwrap_or(-1),
            session,
            &turn,
            &call_id,
        )
        .await;
        if terminal_event == PendingLaunchTerminalEvent::Emit {
            let mut message = format!(
                "process {process_id} could not be stored because its launch was cancelled"
            );
            if let Some(error) = termination_error.as_ref() {
                message = format!("{message}; failed to confirm process termination: {error}");
            }
            if let Some(error) = network_approval_error.as_ref() {
                message = format!("{message}; network approval cleanup failed: {error}");
            }
            emit_failed_exec_end_for_unified_exec(
                Arc::clone(session),
                turn,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                plugin_attribution,
                transcript,
                String::new(),
                message,
                Instant::now().saturating_duration_since(started_at),
            )
            .await;
        }
        session
            .services
            .unified_exec_manager
            .release_process_reservation(process_id_lease)
            .await;
    }
    PendingLaunchOutcome {
        termination_error,
        network_approval_error,
    }
}
