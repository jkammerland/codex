use rand::Rng;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::oneshot::Completion;
use super::pending_launch::ManagerTaskHandle;
use super::pending_launch::PendingLaunch;
use super::pending_launch::PendingLaunchGuard;
use super::pending_launch_supervisor::PendingLaunchResources;
use super::pending_launch_supervisor::spawn_pending_launch_supervisor;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::CODEX_VERSION_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_permission_profile_env;
use crate::exec_env::inject_session_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::guardian::GuardianReviewContext;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::ExecServerEnvConfig;
use crate::tools::ApprovalContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::network_approval::DeferredNetworkApproval;
use crate::tools::network_approval::finish_deferred_network_approval;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::is_managed_proxy_env_var;
use crate::tools::runtimes::unified_exec::UnifiedExecAttempt;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::UnifiedExecRuntime;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::LaunchProcessOwnershipGuard;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessIdLease;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecInteractionEvent;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
#[cfg(test)]
use crate::unified_exec::async_watcher::emit_failed_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::ProcessOperationLease;
use crate::unified_exec::process::SpawnLifecycleHandle;
use crate::unified_exec::process::UnifiedExecProcess;
use crate::unified_exec::shell_snapshot::shell_snapshot_request;
use codex_core_plugins::PLUGIN_METRICS_OUTPUT_ENV_VAR;
use codex_core_plugins::PluginCommandAttribution;
use codex_core_plugins::strip_output_env;
use codex_network_proxy::NetworkPolicyDecider;
use codex_network_proxy::NetworkProxy;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::SandboxErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::shell_environment::is_non_inheritable_env_var;
use codex_sandboxing::SandboxCommand;
use codex_shell_command::is_dangerous_command::DangerousCommandPlatform;
use codex_tools::ToolName;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use codex_utils_path_uri::PathUri;

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];
const NETWORK_ACCESS_DENIED_MESSAGE: &str =
    "Network access was denied by the Codex sandbox network proxy.";
const LATE_NETWORK_DENIAL_GRACE_PERIOD: Duration = Duration::from_millis(100);
const UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE: Duration = Duration::from_secs(5);
const UNIFIED_EXEC_SHUTDOWN_CLEANUP_FAILURE: &str =
    "unified exec process did not finish within the session shutdown cleanup grace";
const INTERRUPT: &str = "\u{3}";

#[derive(Clone, Copy)]
enum ProcessCleanupScope {
    Live,
    SessionShutdown,
}

/// Test-only override for deterministic unified exec process IDs.
///
/// In production builds this value should remain at its default (`false`) and
/// must not be toggled.
static FORCE_DETERMINISTIC_PROCESS_IDS: AtomicBool = AtomicBool::new(false);

pub(super) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    FORCE_DETERMINISTIC_PROCESS_IDS.store(enabled, Ordering::Relaxed);
}

fn deterministic_process_ids_forced_for_tests() -> bool {
    FORCE_DETERMINISTIC_PROCESS_IDS.load(Ordering::Relaxed)
}

fn should_use_deterministic_process_ids() -> bool {
    cfg!(test) || deterministic_process_ids_forced_for_tests()
}

pub(super) fn apply_unified_exec_env(mut env: HashMap<String, String>) -> HashMap<String, String> {
    for (key, value) in UNIFIED_EXEC_ENV {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

pub(super) fn exec_env_policy_from_shell_policy(
    policy: &ShellEnvironmentPolicy,
) -> codex_exec_server::ExecEnvPolicy {
    let mut exclude = policy
        .exclude
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    exclude.extend([
        CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
        CODEX_VERSION_ENV_VAR.to_string(),
        codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
        PLUGIN_METRICS_OUTPUT_ENV_VAR.to_string(),
    ]);
    let mut r#set = policy.r#set.clone();
    r#set.retain(|key, _| {
        ![
            CODEX_PERMISSION_PROFILE_ENV_VAR,
            CODEX_VERSION_ENV_VAR,
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
            PLUGIN_METRICS_OUTPUT_ENV_VAR,
        ]
        .iter()
        .any(|runtime_key| key.eq_ignore_ascii_case(runtime_key))
            && !is_non_inheritable_env_var(key)
    });
    codex_exec_server::ExecEnvPolicy {
        inherit: policy.inherit.clone(),
        ignore_default_excludes: policy.ignore_default_excludes,
        exclude,
        r#set,
        include_only: policy
            .include_only
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

fn env_overlay_for_exec_server(
    request_env: &HashMap<String, String>,
    local_policy_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    request_env
        .iter()
        .filter(|(key, value)| {
            !is_non_inheritable_env_var(key)
                && (matches!(
                    key.as_str(),
                    CODEX_PERMISSION_PROFILE_ENV_VAR
                        | CODEX_VERSION_ENV_VAR
                        | codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR
                ) || local_policy_env.get(*key) != Some(*value))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn exec_server_env_for_request(
    request: &ExecRequest,
) -> (
    Option<codex_exec_server::ExecEnvPolicy>,
    HashMap<String, String>,
) {
    if let Some(exec_server_env_config) = &request.exec_server_env_config {
        let mut env =
            env_overlay_for_exec_server(&request.env, &exec_server_env_config.local_policy_env);
        if request.exec_server_shell_snapshot.is_some()
            && !exec_server_env_config.policy.r#set.contains_key("PATH")
        {
            env.remove("PATH");
        }
        if request.exec_server_managed_network.is_some() {
            for (key, value) in &request.env {
                if is_managed_proxy_env_var(key, value) {
                    env.insert(key.clone(), value.clone());
                }
            }
        }
        (Some(exec_server_env_config.policy.clone()), env)
    } else {
        (None, request.env.clone())
    }
}

fn exec_server_params_for_request(
    process_id: i32,
    request: &ExecRequest,
    tool_ctx: Option<&ToolCtx>,
    windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
    tty: bool,
) -> codex_exec_server::ExecParams {
    let (env_policy, env) = exec_server_env_for_request(request);
    let sandbox = request.exec_server_sandbox.clone().map(|mut sandbox| {
        sandbox.windows_sandbox_proxy_settings_mode = Some(windows_sandbox_proxy_settings_mode);
        sandbox
    });
    // Sandbox retries and memory-backed local launches can reuse a unified-exec
    // ID while the executor still retains the previous process.
    let exec_server_process_id =
        if request.exec_server_sandbox.is_some() || request.exec_server_shell_snapshot.is_some() {
            format!("{process_id}-{}", Uuid::new_v4())
        } else {
            process_id.to_string()
        };
    codex_exec_server::ExecParams {
        process_id: exec_server_process_id.into(),
        metadata: tool_ctx.map(|ctx| codex_exec_server::ExecMetadata {
            thread_id: Some(ctx.session.thread_id()),
            tool_call_id: Some(ctx.call_id.clone()),
        }),
        argv: request.command.clone(),
        cwd: request.cwd.clone(),
        env_policy,
        shell_snapshot: request.exec_server_shell_snapshot.clone(),
        env,
        tty,
        pipe_stdin: false,
        arg0: request.arg0.clone(),
        sandbox,
        enforce_managed_network: request.exec_server_enforce_managed_network,
        managed_network: request.exec_server_managed_network.clone(),
        network_proxy: request.exec_server_network_proxy.clone(),
    }
}

/// Borrowed process state prepared for a `write_stdin` or poll operation.
struct PreparedProcessHandles {
    process: Arc<UnifiedExecProcess>,
    output: OutputHandles,
    pause_state: Option<watch::Receiver<bool>>,
    session: Option<Arc<crate::session::session::Session>>,
    network_approval: Option<DeferredNetworkApproval>,
    call_id: String,
    hook_command: String,
    process_id: i32,
    tty: bool,
}

struct InitialExecCommandGuard {
    active: Option<Arc<AtomicBool>>,
}

impl Drop for InitialExecCommandGuard {
    fn drop(&mut self) {
        if let Some(active) = self.active.as_ref() {
            active.store(false, Ordering::Release);
        }
    }
}

struct PreparedPendingLaunch {
    request: ExecCommandRequest,
    process: Arc<UnifiedExecProcess>,
    permissions: super::TerminalPermissions,
    deferred_network_approval: Option<DeferredNetworkApproval>,
    started_at: Instant,
    plugin_attribution: Option<PluginCommandAttribution>,
    pending_launch_guard: PendingLaunchGuard,
}

async fn unregister_network_approval_for_entry(entry: &ProcessEntry) {
    if let Some(network_approval) = entry.network_approval.as_ref()
        && let Some(session) = entry.session.upgrade()
    {
        session
            .services
            .network_approval
            .unregister_call(network_approval.registration_id())
            .await;
    }
}

async fn finish_network_approval_after_process_exit_for_entry(
    entry: &ProcessEntry,
) -> Result<(), String> {
    let session = entry.session.upgrade();
    finish_deferred_network_approval_after_process_exit_for_session(
        session.as_ref(),
        entry.network_approval.clone(),
    )
    .await
}

pub(super) async fn finish_deferred_network_approval_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    let Some(session) = session else {
        return Ok(());
    };
    finish_deferred_network_approval(session.as_ref(), deferred)
        .await
        .map_err(network_approval_error_message)
}

fn network_approval_error_message(err: ToolError) -> String {
    match err {
        ToolError::Rejected(message) => message,
        ToolError::Codex(err) => err.to_string(),
    }
}

async fn network_denial_message_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> String {
    let Some(session) = session else {
        return NETWORK_ACCESS_DENIED_MESSAGE.to_string();
    };
    match finish_deferred_network_approval(session.as_ref(), deferred).await {
        Ok(()) => NETWORK_ACCESS_DENIED_MESSAGE.to_string(),
        Err(err) => network_approval_error_message(err),
    }
}

async fn wait_for_late_network_denial(network_cancelled: Option<CancellationToken>) -> bool {
    let Some(network_cancelled) = network_cancelled else {
        return false;
    };
    if network_cancelled.is_cancelled() {
        return true;
    }

    tokio::select! {
        _ = network_cancelled.cancelled() => true,
        _ = tokio::time::sleep(LATE_NETWORK_DENIAL_GRACE_PERIOD) => false,
    }
}

async fn finish_deferred_network_approval_after_process_exit_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    wait_for_late_network_denial(
        deferred
            .as_ref()
            .map(DeferredNetworkApproval::cancellation_token),
    )
    .await;
    finish_deferred_network_approval_for_session(session, deferred).await
}

fn fail_process_with_message(process: &UnifiedExecProcess, message: String) -> UnifiedExecError {
    if let Some(message) = process.failure_message() {
        process.terminate();
        return UnifiedExecError::process_failed(message);
    }

    process.fail_and_terminate(message.clone());
    UnifiedExecError::process_failed(process.failure_message().unwrap_or(message))
}

async fn fail_process_after_message_resolution(
    process: &UnifiedExecProcess,
    resolve_message: impl Future<Output = String>,
) -> UnifiedExecError {
    let message = resolve_message.await;
    fail_process_with_message(process, message)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn emit_failed_initial_exec_end_if_unstored(
    process_started_alive: bool,
    context: &UnifiedExecContext,
    request: &ExecCommandRequest,
    cwd: PathUri,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    wall_time: Duration,
) {
    if process_started_alive {
        return;
    }

    emit_failed_exec_end_for_unified_exec(
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id.clone(),
        request.command.clone(),
        cwd,
        Some(request.process_id_lease.process_id().to_string()),
        plugin_attribution,
        transcript,
        fallback_output,
        message,
        wall_time,
    )
    .await;
}

fn terminate_process_on_network_denial(
    process: Arc<UnifiedExecProcess>,
    session: std::sync::Weak<crate::session::session::Session>,
    deferred: DeferredNetworkApproval,
    pending_launch: &Arc<PendingLaunch>,
) -> ManagerTaskHandle {
    let network_cancelled = deferred.cancellation_token();
    let process_exited = process.cancellation_token();
    pending_launch.spawn_manager_task(async move {
        let denied = tokio::select! {
            _ = network_cancelled.cancelled() => true,
            _ = process_exited.cancelled() => {
                wait_for_late_network_denial(Some(network_cancelled.clone())).await
            }
        };
        if !denied {
            return;
        }
        let session = session.upgrade();
        let _ = fail_process_after_message_resolution(
            process.as_ref(),
            network_denial_message_for_session(session.as_ref(), Some(deferred)),
        )
        .await;
    })
}

impl UnifiedExecProcessManager {
    pub(crate) async fn retain_existing_process_ids(&self, process_ids: Vec<i32>) -> Vec<i32> {
        let store = self.process_store.lock().await;
        process_ids
            .into_iter()
            .filter(|process_id| store.processes.contains_key(process_id))
            .collect()
    }

    pub(crate) async fn allocate_process_id(&self) -> ProcessIdLease {
        loop {
            let mut store = self.process_store.lock().await;

            let process_id = if should_use_deterministic_process_ids() {
                // Test or deterministic mode. Keep a monotonic cursor so safely
                // releasing reservation tokens does not make IDs repeat.
                let process_id = store.next_deterministic_process_id;
                store.next_deterministic_process_id = process_id.checked_add(1).unwrap_or(1000);
                process_id
            } else {
                // production mode → random
                rand::rng().random_range(1_000..100_000)
            };

            if store.reserved_process_ids.contains_key(&process_id)
                || store.processes.contains_key(&process_id)
            {
                continue;
            }

            let process_id_lease =
                ProcessIdLease::new_reserved(process_id, &store.reserved_process_ids);
            store
                .reserved_process_ids
                .insert(process_id, process_id_lease.clone());
            return process_id_lease;
        }
    }

    pub(crate) async fn release_process_id(&self, process_id_lease: &ProcessIdLease) {
        let removed = {
            let mut store = self.process_store.lock().await;
            store.remove(process_id_lease)
        };
        if let Some(entry) = removed {
            unregister_network_approval_for_entry(&entry).await;
        }
    }

    pub(super) async fn release_process_reservation(&self, process_id_lease: &ProcessIdLease) {
        self.process_store
            .lock()
            .await
            .release_reservation(process_id_lease);
    }

    async fn remove_process_if_matches(&self, expected_process: &Arc<UnifiedExecProcess>) {
        let Some(process_id_lease) = expected_process.process_id_lease() else {
            return;
        };
        self.process_store
            .lock()
            .await
            .remove_if_process_matches(process_id_lease, expected_process);
    }

    async fn acquire_process_operation(
        &self,
        process_id: i32,
    ) -> Result<(Arc<UnifiedExecProcess>, ProcessOperationLease), UnifiedExecError> {
        let store = self.process_store.lock().await;
        let process = Arc::clone(
            &store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?
                .process,
        );
        // Acquire while the store is still locked. Pruning uses the same store
        // lock before checking this lease, closing the lookup-to-operation-lock
        // eviction window for both wait_process and write_stdin.
        let operation_lease = process.acquire_operation_lease();
        Ok((process, operation_lease))
    }

    async fn begin_pending_launch(
        &self,
        process_id_lease: &ProcessIdLease,
    ) -> Result<PendingLaunchGuard, UnifiedExecError> {
        let store = self.process_store.lock().await;
        if !store.reservation_matches(process_id_lease) {
            return Err(UnifiedExecError::UnknownProcessId {
                process_id: process_id_lease.process_id(),
            });
        }
        process_id_lease
            .pending_launch()
            .begin()
            .map_err(|_| UnifiedExecError::UnknownProcessId {
                process_id: process_id_lease.process_id(),
            })
    }

    async fn prepare_pending_launch(
        mut request: ExecCommandRequest,
        context: UnifiedExecContext,
        cwd: PathUri,
        plugin_attribution: Option<PluginCommandAttribution>,
        pending_launch_guard: PendingLaunchGuard,
    ) -> Result<PreparedPendingLaunch, UnifiedExecError> {
        let manager = &context.session.services.unified_exec_manager;
        let process_id = request.process_id_lease.process_id();
        let process = manager
            .open_session_with_sandbox(&request, cwd.clone(), &context)
            .await;
        let (attempt, deferred_network_approval) = match process {
            Ok(result) => result,
            Err(error) => {
                manager.release_process_id(&request.process_id_lease).await;
                return Err(error);
            }
        };
        let UnifiedExecAttempt {
            process,
            metrics_sidecar,
            permissions,
        } = attempt;
        let process = Arc::new(process);
        process.attach_process_id_lease(request.process_id_lease.clone());
        let mut network_denial_monitor = deferred_network_approval.as_ref().map(|deferred| {
            terminate_process_on_network_denial(
                Arc::clone(&process),
                Arc::downgrade(&context.session),
                deferred.clone(),
                request.process_id_lease.pending_launch(),
            )
        });
        let transcript = process.output_transcript();
        let started_at = Instant::now();
        start_streaming_output(
            &process,
            &context,
            Arc::clone(request.process_id_lease.pending_launch()),
        )
        .await;
        let plugin_metrics_sidecar =
            metrics_sidecar.map(|sidecar| Arc::new(std::sync::Mutex::new(Some(sidecar))));
        let pending_decision = match pending_launch_guard.launch().publish() {
            Ok(pending_decision) => pending_decision,
            Err(_) => {
                if let Some(network_denial_monitor) = network_denial_monitor.take() {
                    network_denial_monitor.cancel_and_wait().await;
                }
                if !process.has_exited()
                    && let Err(error) = process.terminate_confirmed().await
                {
                    process.mark_termination_failure(error.to_string());
                }
                let _ = finish_deferred_network_approval_for_session(
                    Some(&context.session),
                    deferred_network_approval,
                )
                .await;
                manager.release_process_id(&request.process_id_lease).await;
                return Err(UnifiedExecError::UnknownProcessId { process_id });
            }
        };
        spawn_pending_launch_supervisor(
            request.process_id_lease.clone(),
            pending_decision,
            PendingLaunchResources {
                process: Arc::clone(&process),
                session: Arc::downgrade(&context.session),
                turn: Arc::clone(&context.step_context.turn),
                call_id: context.call_id.clone(),
                command: request.command.clone(),
                cwd: cwd.clone(),
                process_id,
                plugin_attribution: plugin_attribution.clone(),
                transcript: Arc::clone(&transcript),
                started_at,
                network_approval: deferred_network_approval.clone(),
                network_denial_monitor,
                plugin_metrics_sidecar: plugin_metrics_sidecar.clone(),
            },
        );
        // From this point onward the supervisor plus PendingLaunchGuard own
        // cancellation cleanup. The allocation guard is only for the earlier
        // windows where dropping the originating future otherwise leaked the
        // reservation.
        request.process_id_lease.disarm_auto_release();

        Ok(PreparedPendingLaunch {
            request,
            process,
            permissions,
            deferred_network_approval,
            started_at,
            plugin_attribution,
            pending_launch_guard,
        })
    }

    pub(crate) async fn exec_command(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        self.exec_command_inner(request, context, /*completion*/ None)
            .await
    }

    pub(super) async fn exec_command_inner(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
        mut completion: Option<&mut Completion<'_>>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let cwd = request.cwd.clone();
        let process_id = request.process_id_lease.process_id();
        let process_id_lease = request.process_id_lease.clone();
        let pending_launch_guard = self.begin_pending_launch(&request.process_id_lease).await?;
        // Resolve attribution before spawning. Backend start and publication
        // then run in a manager-owned task, so cancelling this tool future
        // cannot abandon a start that has not returned its process handle yet.
        let plugin_attribution = if request.turn_environment.environment.is_remote() {
            let file_system = request.turn_environment.environment.get_filesystem();
            context
                .step_context
                .turn
                .plugin_attribution_for_executor_command(
                    &request.command,
                    &cwd,
                    file_system.as_ref(),
                )
                .await
        } else {
            cwd.to_abs_path().ok().and_then(|cwd| {
                context
                    .step_context
                    .turn
                    .plugin_attribution_for_command(&request.command, &cwd)
            })
        };
        let launch_context = UnifiedExecContext::new(
            Arc::clone(&context.session),
            Arc::clone(&context.step_context),
            context.cancellation_token.clone(),
            context.call_id.clone(),
        );
        let launch_cwd = cwd.clone();
        let launch_rx = pending_launch_guard.spawn_manager_owned(move |pending_launch_guard| {
            Self::prepare_pending_launch(
                request,
                launch_context,
                launch_cwd,
                plugin_attribution,
                pending_launch_guard,
            )
        });
        let prepared = match launch_rx.await {
            Ok(result) => result?,
            Err(_) => {
                self.release_process_id(&process_id_lease).await;
                return Err(UnifiedExecError::process_failed(
                    "manager-owned process launch ended without a result".to_string(),
                ));
            }
        };
        let PreparedPendingLaunch {
            request,
            process,
            permissions,
            deferred_network_approval,
            started_at: start,
            plugin_attribution,
            mut pending_launch_guard,
        } = prepared;
        if let Some(completion) = completion.as_ref() {
            let _ = completion.process.set(Arc::clone(&process));
        }

        let begin_session = Arc::clone(&context.session);
        let begin_turn = Arc::clone(&context.step_context.turn);
        let begin_call_id = context.call_id.clone();
        let emitter = ToolEmitter::unified_exec(
            &request.command,
            cwd.clone(),
            ExecCommandSource::UnifiedExecStartup,
            Some(process_id.to_string()),
            plugin_attribution.clone(),
        );
        let begin_rx = pending_launch_guard.spawn_manager_owned_begin(async move {
            let event_ctx = ToolEventCtx::new(
                begin_session.as_ref(),
                begin_turn.as_ref(),
                &begin_call_id,
                /*turn_diff_tracker*/ None,
            );
            emitter.emit(event_ctx, ToolEventStage::Begin).await;
        });
        let begin_result = match begin_rx {
            Ok(begin_rx) => begin_rx.await,
            Err(_) => {
                request.process_id_lease.pending_launch().wait().await;
                return Err(UnifiedExecError::UnknownProcessId { process_id });
            }
        };
        match begin_result {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                request.process_id_lease.pending_launch().wait().await;
                return Err(UnifiedExecError::process_failed(
                    "manager-owned process start event ended without committing".to_string(),
                ));
            }
        }

        // Persist live sessions before the initial yield wait so interrupting the
        // turn cannot drop the last Arc and terminate the background process.
        let process_started_alive = !process.has_exited() && process.exit_code().is_none();
        if !process_started_alive {
            pending_launch_guard
                .watch_process()
                .map_err(|_| UnifiedExecError::UnknownProcessId { process_id })?;
        }
        let _initial_exec_command_guard = if process_started_alive {
            let initial_exec_command_active = Arc::new(AtomicBool::new(true));
            self.store_process(
                Arc::clone(&process),
                context,
                request.hook_command.clone(),
                cwd.clone(),
                request.turn_environment.selection.environment_id.clone(),
                permissions,
                start,
                request.process_id_lease.clone(),
                request.tty,
                deferred_network_approval.clone(),
                Arc::clone(&initial_exec_command_active),
                &mut pending_launch_guard,
            )
            .await?;
            InitialExecCommandGuard {
                active: Some(initial_exec_command_active),
            }
        } else {
            InitialExecCommandGuard { active: None }
        };

        let yield_time_ms = clamp_yield_time(request.yield_time_ms);
        // For the initial exec_command call, we both stream output to events
        // (via start_streaming_output above) and collect a snapshot here for
        // the tool response body.
        let wait = completion.as_ref().map_or_else(
            || Duration::from_millis(yield_time_ms),
            |completion| completion.timeout,
        );
        let deadline = start
            .checked_add(wait)
            .ok_or_else(|| UnifiedExecError::process_failed("timeout_ms is too large".into()))?;
        let collected_output = Self::collect_output_until_deadline(
            process.output_handles(),
            Some(context.session.subscribe_elicitation_pause_state()),
            deadline,
        )
        .await;
        if let Some(completion) = completion.as_mut()
            && !process.has_exited()
        {
            completion.timed_out = true;
            process.mark_timed_out();
            if let Err(err) = process.terminate_confirmed().await {
                process.fail_and_terminate(err.to_string());
            }
        }
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let text = String::from_utf8_lossy(&collected).to_string();
        let chunk_id = generate_chunk_id();
        if deferred_network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let error = fail_process_after_message_resolution(
                process.as_ref(),
                network_denial_message_for_session(
                    Some(&context.session),
                    deferred_network_approval.clone(),
                ),
            )
            .await;
            self.remove_process_if_matches(&process).await;
            if !process_started_alive {
                let cleanup = request.process_id_lease.pending_launch().wait().await;
                if let Some(message) = cleanup.network_approval_error {
                    return Err(UnifiedExecError::process_failed(message));
                }
            }
            return Err(error);
        }
        if let Some(message) = process.failure_message() {
            self.remove_process_if_matches(&process).await;
            if !process_started_alive {
                let cleanup = request.process_id_lease.pending_launch().wait().await;
                if let Some(message) = cleanup.network_approval_error {
                    return Err(fail_process_with_message(process.as_ref(), message));
                }
            }
            return Err(UnifiedExecError::process_failed(message));
        }
        let (response_process_id, exit_code) = if process_started_alive {
            match self.refresh_process_state(process_id, &process).await {
                ProcessStatus::Alive {
                    exit_code,
                    process_id,
                    ..
                } => (Some(process_id), exit_code),
                ProcessStatus::Exited { exit_code, entry } => {
                    let cleanup = entry.process_id_lease.pending_launch().wait().await;
                    if let Some(message) = cleanup.network_approval_error {
                        return Err(fail_process_with_message(entry.process.as_ref(), message));
                    }
                    if let Some(message) = entry.process.failure_message() {
                        return Err(UnifiedExecError::process_failed(message));
                    }
                    if !completion
                        .as_ref()
                        .is_some_and(|completion| completion.timed_out)
                    {
                        process
                            .check_for_sandbox_denial_with_text(&text)
                            .await
                            .map_err(|err| {
                                err.with_output_collection_metadata(
                                    original_token_count,
                                    output_omitted_bytes,
                                )
                            })?;
                    }
                    (None, exit_code)
                }
                ProcessStatus::Unknown => {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        } else {
            // The manager-owned watcher performs all terminal side effects,
            // including network cleanup, metrics, lifecycle events, and final
            // reservation release. Origin cancellation cannot split them.
            let cleanup = request.process_id_lease.pending_launch().wait().await;
            if let Some(message) = cleanup.network_approval_error {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            if let Some(message) = process.failure_message() {
                return Err(UnifiedExecError::process_failed(message));
            }
            let exit_code = process.exit_code();
            process
                .check_for_sandbox_denial_with_text(&text)
                .await
                .map_err(|err| {
                    err.with_output_collection_metadata(original_token_count, output_omitted_bytes)
                })?;
            (None, exit_code)
        };

        let response = ExecCommandToolOutput {
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: context
                .step_context
                .turn
                .model_info()
                .truncation_policy
                .into(),
            max_output_tokens: request.max_output_tokens,
            process_id: response_process_id,
            exit_code,
            wait_reason: None,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(request.hook_command.clone()),
        };

        Ok(response)
    }

    pub(crate) async fn write_stdin(
        &self,
        context: &UnifiedExecContext,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let process_id = request.process_id;

        // Different terminal sessions can be polled concurrently, but reads and
        // writes against one terminal must not overlap because they share a
        // draining output buffer and process lifecycle.
        let (locked_process, _operation_lease) = self.acquire_process_operation(process_id).await?;
        let _interaction_guard = locked_process.interaction_lock().lock_owned().await;
        // A queued write must observe strict review enabled while it was waiting.
        let strict_auto_review = context
            .session
            .active_turn_context_and_strict_auto_review()
            .await
            .is_some_and(|(_, _, strict)| strict);
        let approval = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .filter(|entry| Arc::ptr_eq(&entry.process, &locked_process))
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            entry.stdin_approval(context, request.input, strict_auto_review)?
        };
        if let Some((approval, approval_reason)) = approval {
            let reviewed = crate::guardian::format_guardian_action_pretty(
                &approval.clone().into_guardian_request().map_err(|err| {
                    UnifiedExecError::StdinApproval(ToolError::Rejected(err.to_string()))
                })?,
            )
            .map_err(|err| UnifiedExecError::StdinApproval(ToolError::Rejected(err.to_string())))?;
            // Bound the entire serialized action plus its reason, including JSON
            // escaping. Reject, never execute an unreviewed tail.
            let oversized = reviewed.text.len().saturating_add(approval_reason.len()) > 8_000;
            let size_check_result = if reviewed.truncated {
                "formatter_truncated"
            } else if oversized {
                "over_limit"
            } else {
                "within_limit"
            };
            let input_kind = if request.input.chars().all(char::is_control) {
                "control"
            } else {
                "text"
            };
            context.step_context.session_telemetry.counter(
                "codex.unified_exec.stdin_review.size_check",
                /*inc*/ 1,
                &[("result", size_check_result), ("input_kind", input_kind)],
            );
            if reviewed.truncated || oversized {
                return Err(UnifiedExecError::StdinApproval(ToolError::Rejected(
                    "terminal input and permission details are too large to review safely; use a smaller input or start a new terminal with fewer grants".to_string(),
                )));
            }
            let approval_context = ApprovalContext {
                review_context: GuardianReviewContext::from(&context.step_context),
                cancellation_token: Some(context.cancellation_token.clone()),
                call_id: context.call_id.clone(),
                tool_name: ToolName::plain("write_stdin"),
                strict_auto_review,
                approval_reason: Some(approval_reason),
                retry_reason: None,
                network_approval_context: None,
            };
            context
                .session
                .request_approval(approval, approval_context)
                .await
                .map_err(UnifiedExecError::StdinApproval)?;
        }

        // Revalidate the identity after approval: a removed process ID can be reused.
        let PreparedProcessHandles {
            process,
            output,
            pause_state,
            session,
            network_approval,
            call_id,
            hook_command,
            process_id,
            tty,
            ..
        } = self
            .prepare_process_handles(process_id, &locked_process)
            .await?;
        let mut status_after_write = None;

        if !request.input.is_empty() {
            if !tty {
                if request.input == INTERRUPT {
                    process.interrupt().await?;
                } else {
                    return Err(UnifiedExecError::StdinClosed);
                }
            } else {
                match process.write(request.input.as_bytes()).await {
                    Ok(()) => {
                        // Give the remote process a brief window to react so that we are
                        // more likely to capture its output in the poll below.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(err) => {
                        let status = self.refresh_process_state(process_id, &process).await;
                        if matches!(status, ProcessStatus::Exited { .. }) {
                            status_after_write = Some(status);
                        } else if matches!(err, UnifiedExecError::ProcessFailed { .. }) {
                            process.terminate();
                            self.remove_process_if_matches(&process).await;
                            return Err(err);
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
        }

        let yield_time_ms = {
            // Empty polls use configurable background timeout bounds. Non-empty
            // writes keep a fixed max cap so interactive stdin remains responsive.
            let time_ms = request.yield_time_ms.max(MIN_YIELD_TIME_MS);
            if request.input.is_empty() {
                time_ms.clamp(MIN_EMPTY_YIELD_TIME_MS, self.max_write_stdin_yield_time_ms)
            } else {
                time_ms.min(MAX_YIELD_TIME_MS)
            }
        };
        let start = Instant::now();
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected_output =
            Self::collect_output_until_deadline(&output, pause_state, deadline).await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let chunk_id = generate_chunk_id();
        if network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message =
                network_denial_message_for_session(session.as_ref(), network_approval.clone())
                    .await;
            self.remove_process_if_matches(&process).await;
            return Err(fail_process_with_message(process.as_ref(), message));
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                session.as_ref(),
                network_approval.clone(),
            )
            .await;
            self.remove_process_if_matches(&process).await;
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag or suppress TerminalInteraction
        // with an appropriate process_id and exit_code.
        let status = if let Some(status) = status_after_write {
            status
        } else {
            self.refresh_process_state(process_id, &process).await
        };
        let (process_id, exit_code, event_call_id) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id),
            ProcessStatus::Exited { exit_code, entry } => {
                let call_id = entry.call_id.clone();
                if let Err(message) =
                    finish_network_approval_after_process_exit_for_entry(&entry).await
                {
                    return Err(fail_process_with_message(entry.process.as_ref(), message));
                }
                (None, exit_code, call_id)
            }
            ProcessStatus::Unknown => {
                if process.has_exited() {
                    (None, process.exit_code(), call_id)
                } else {
                    return Err(UnifiedExecError::UnknownProcessId {
                        process_id: request.process_id,
                    });
                }
            }
        };

        let response = ExecCommandToolOutput {
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            process_id,
            exit_code,
            wait_reason: None,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(hook_command),
        };

        let should_emit_interaction = !request.input.is_empty() || response.process_id.is_some();
        if should_emit_interaction
            && let Some(UnifiedExecInteractionEvent { session, turn }) = request.interaction_event
        {
            let interaction = TerminalInteractionEvent {
                call_id: response.event_call_id.clone(),
                process_id: response
                    .process_id
                    .unwrap_or(request.process_id)
                    .to_string(),
                stdin: request.input.to_string(),
            };
            session
                .send_event(turn.as_ref(), EventMsg::TerminalInteraction(interaction))
                .await;
        }

        Ok(response)
    }

    async fn refresh_process_state(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> ProcessStatus {
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get_mut(&process_id) else {
            return ProcessStatus::Unknown;
        };
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return ProcessStatus::Unknown;
        }

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id;
        let process_id_lease = entry.process_id_lease.clone();

        if entry.process.has_exited() {
            let Some(entry) = store.remove_if_process_matches(&process_id_lease, expected_process)
            else {
                return ProcessStatus::Unknown;
            };
            ProcessStatus::Exited {
                exit_code,
                entry: Box::new(entry),
            }
        } else {
            ProcessStatus::Alive {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        }
    }

    async fn prepare_process_handles(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> Result<PreparedProcessHandles, UnifiedExecError> {
        let mut store = self.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return Err(UnifiedExecError::UnknownProcessId { process_id });
        }
        entry.last_used = Instant::now();
        let output = entry.process.output_handles().clone();
        let pause_state = entry
            .session
            .upgrade()
            .map(|session| session.subscribe_elicitation_pause_state());
        let session = entry.session.upgrade();

        Ok(PreparedProcessHandles {
            process: Arc::clone(&entry.process),
            output,
            pause_state,
            session,
            network_approval: entry.network_approval.clone(),
            call_id: entry.call_id.clone(),
            hook_command: entry.hook_command.clone(),
            process_id: entry.process_id,
            tty: entry.tty,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        hook_command: String,
        cwd: PathUri,
        environment_id: String,
        permissions: super::TerminalPermissions,
        started_at: Instant,
        process_id_lease: ProcessIdLease,
        tty: bool,
        network_approval: Option<DeferredNetworkApproval>,
        initial_exec_command_active: Arc<AtomicBool>,
        pending_launch_guard: &mut PendingLaunchGuard,
    ) -> Result<(), UnifiedExecError> {
        let process_id = process_id_lease.process_id();
        let attached_allocation_matches = process
            .process_id_lease()
            .is_some_and(|attached| attached.same_allocation(&process_id_lease));
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            process_id_lease: process_id_lease.clone(),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone(),
            initial_exec_command_active,
            hook_command,
            tty,
            environment_id,
            permissions,
            network_approval,
            session: Arc::downgrade(&context.session),
            last_used: started_at,
        };
        let admission = if attached_allocation_matches {
            let mut store = self.process_store.lock().await;
            if !store.reservation_matches(&entry.process_id_lease)
                || store.processes.contains_key(&process_id)
            {
                Err(entry)
            } else {
                let pruned_entry = Self::prune_processes_if_needed(&mut store);
                match store.insert(entry) {
                    Ok(()) => match pending_launch_guard.watch_process() {
                        Ok(()) => Ok(pruned_entry),
                        Err(_) => {
                            let Some(rejected_entry) =
                                store.remove_if_process_matches(&process_id_lease, &process)
                            else {
                                unreachable!(
                                    "newly stored process should still match its allocation"
                                );
                            };
                            Err(rejected_entry)
                        }
                    },
                    Err(entry) => Err(*entry),
                }
            }
        } else {
            Err(entry)
        };
        let pruned_entry = match admission {
            Ok(pruned_entry) => pruned_entry,
            Err(rejected_entry) => {
                drop(rejected_entry);
                let store_error = UnifiedExecError::UnknownProcessId { process_id };
                pending_launch_guard.launch().request_cleanup();
                let cleanup = pending_launch_guard.launch().wait().await;
                if let Some(message) = cleanup.termination_error {
                    return Err(UnifiedExecError::process_failed(message));
                }
                if let Some(message) = cleanup.network_approval_error {
                    return Err(UnifiedExecError::process_failed(message));
                }
                return Err(store_error);
            }
        };
        // The pending-launch supervisor owns approval cleanup, terminal events,
        // metrics, and the retained reservation. Trigger termination without an
        // await so cancellation cannot strand the entry after it is pruned.
        if let Some(pruned_entry) = pruned_entry {
            pruned_entry.process.terminate();
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_exec_env(
        &self,
        process_id: i32,
        tool_ctx: &ToolCtx,
        command: SandboxCommand,
        options: ExecOptions,
        attempt: &SandboxAttempt<'_>,
        network: Option<&NetworkProxy>,
        network_proxy_launch: Option<codex_network_proxy::RemoteNetworkProxyLaunchConfig>,
        environment_id: Option<&str>,
        exec_server_env_config: Option<ExecServerEnvConfig>,
        shell_snapshot: Option<codex_exec_server::ShellSnapshotRequest>,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
        tty: bool,
        spawn_lifecycle: SpawnLifecycleHandle,
        environment: &codex_exec_server::Environment,
    ) -> Result<UnifiedExecProcess, ToolError> {
        let mut request = if environment.is_remote() || shell_snapshot.is_some() {
            attempt.env_for_exec_server(command, options)
        } else {
            attempt.env_for(command, options, network, environment_id)
        }
        .map_err(ToolError::Codex)?;
        let network_policy_decider = network_proxy_launch
            .as_ref()
            .filter(|launch| launch.policy_decision_timeout_ms.is_some())
            .and_then(|_| network.and_then(NetworkProxy::remote_policy_decider));
        if environment.is_remote() && network.is_some() && network_proxy_launch.is_none() {
            request.exec_server_enforce_managed_network = false;
        }
        request.exec_server_network_proxy = network_proxy_launch;
        request.exec_server_env_config = exec_server_env_config;
        request.exec_server_shell_snapshot = shell_snapshot;
        self.open_session_with_prepared_exec_env(
            process_id,
            &request,
            Some(tool_ctx),
            windows_sandbox_proxy_settings_mode,
            network_policy_decider,
            tty,
            spawn_lifecycle,
            environment,
        )
        .await
        .map_err(|err| match err {
            UnifiedExecError::SandboxDenied { output, .. } => {
                ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output: Box::new(output),
                    network_policy_decision: None,
                }))
            }
            other => ToolError::Rejected(other.to_string()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_prepared_exec_env(
        &self,
        process_id: i32,
        request: &ExecRequest,
        tool_ctx: Option<&ToolCtx>,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
        network_policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
        tty: bool,
        mut spawn_lifecycle: SpawnLifecycleHandle,
        environment: &codex_exec_server::Environment,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        let inherited_fds = spawn_lifecycle.inherited_fds();

        if environment.is_remote() || request.exec_server_shell_snapshot.is_some() {
            if !inherited_fds.is_empty() {
                return Err(UnifiedExecError::create_process(
                    "remote exec-server does not support inherited file descriptors".to_string(),
                ));
            }

            // Capture launch ownership before awaiting the backend. Session
            // shutdown may clear the reservation registry while start is in
            // flight, but a returned process must still attach to the exact
            // pending launch before any fallible post-start validation.
            let process_id_lease = self
                .launch_process_ownership
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&process_id)
                .cloned();
            let backend = environment.get_exec_backend();
            let params = exec_server_params_for_request(
                process_id,
                request,
                tool_ctx,
                windows_sandbox_proxy_settings_mode,
                tty,
            );
            let started = match network_policy_decider {
                Some(decider) => {
                    backend
                        .start_with_network_policy_decider(params, decider)
                        .await
                }
                None => backend.start(params).await,
            }
            .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
            spawn_lifecycle.after_spawn();
            return match process_id_lease {
                Some(process_id_lease) => {
                    UnifiedExecProcess::from_exec_server_started_for_pending_launch(
                        started,
                        process_id_lease,
                    )
                    .await
                }
                None => UnifiedExecProcess::from_exec_server_started(started).await,
            };
        }

        // TODO(anp): Keep PathUri through the local PTY/process launch boundary.
        let native_cwd = request
            .cwd
            .to_abs_path()
            .map_err(|_| UnifiedExecError::ForeignPath {
                path: request.cwd.clone(),
            })?;

        if request.command.is_empty() {
            return Err(UnifiedExecError::MissingCommandLine);
        }
        let network_proxy_restricting_sid = {
            #[cfg(target_os = "windows")]
            {
                if request.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
                    request
                        .network
                        .as_ref()
                        .map(|network| {
                            network
                                .network_proxy_restricting_sid(
                                    request.network_environment_id.as_deref(),
                                )
                                .ok_or_else(|| {
                                    UnifiedExecError::create_process(
                                        "managed Windows proxy route is missing its restricting SID"
                                            .to_string(),
                                    )
                                })
                        })
                        .transpose()?
                } else {
                    None
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                None::<String>
            }
        };
        let windows_sandbox =
            if request.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
                Some(codex_sandboxing::WindowsSandboxSpawnRequest {
                    permission_profile: &request.permission_profile,
                    workspace_roots: &request.windows_sandbox_workspace_roots,
                    windows_sandbox_level: request.windows_sandbox_level,
                    proxy_enforced: request.network.is_some(),
                    network_proxy_restricting_sid: network_proxy_restricting_sid.as_deref(),
                    proxy_settings_mode: windows_sandbox_proxy_settings_mode,
                    filesystem_overrides: request.windows_sandbox_filesystem_overrides.as_ref(),
                    use_private_desktop: request.windows_sandbox_private_desktop,
                })
            } else {
                None
            };
        let spawn_result = codex_sandboxing::spawn_process(codex_sandboxing::SpawnRequest {
            command: &request.command,
            cwd: native_cwd.as_path(),
            env: &request.env,
            arg0: &request.arg0,
            sandbox: request.sandbox,
            windows_sandbox,
            tty,
            stdin_open: tty,
            inherited_fds: &inherited_fds,
        })
        .await;
        spawn_lifecycle.after_spawn();
        let spawned =
            spawn_result.map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
        UnifiedExecProcess::from_spawned(spawned, request.sandbox, spawn_lifecycle).await
    }

    pub(super) async fn open_session_with_sandbox(
        &self,
        request: &ExecCommandRequest,
        cwd: PathUri,
        context: &UnifiedExecContext,
    ) -> Result<(UnifiedExecAttempt, Option<DeferredNetworkApproval>), UnifiedExecError> {
        let replaced_ownership = self
            .launch_process_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                request.process_id_lease.process_id(),
                request.process_id_lease.clone(),
            );
        debug_assert!(
            replaced_ownership
                .as_ref()
                .is_none_or(|lease| { lease.same_allocation(&request.process_id_lease) })
        );
        let _launch_process_ownership = LaunchProcessOwnershipGuard {
            ownership: &self.launch_process_ownership,
            process_id_lease: request.process_id_lease.clone(),
        };
        let turn = &context.step_context.turn;
        let shell_environment_policy = request.turn_environment.shell_environment_policy();
        let local_policy_env = create_env(shell_environment_policy, /*thread_id*/ None);
        let mut env = local_policy_env.clone();
        env.insert(
            CODEX_THREAD_ID_ENV_VAR.to_string(),
            context.session.thread_id.to_string(),
        );
        inject_session_env(&mut env, context.session.session_id());
        inject_apply_patch_env(&mut env, &turn.config.features);
        let active_permission_profile = request.turn_environment.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());
        let mut env = apply_unified_exec_env(env);
        strip_output_env(&mut env);
        let mut explicit_env_overrides = shell_environment_policy.r#set.clone();
        strip_output_env(&mut explicit_env_overrides);
        let exec_server_env_config = ExecServerEnvConfig {
            policy: exec_env_policy_from_shell_policy(shell_environment_policy),
            local_policy_env,
        };
        let shell_snapshot = shell_snapshot_request(request, &cwd, context);
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = UnifiedExecRuntime::new(self, request.shell_mode.clone());
        let session_shell = context.session.user_shell();
        let configured_shell = request
            .turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        // Legacy executors omit the platform. Unknown reported platforms use the
        // more conservative Windows rules rather than silently weakening checks.
        let command_platform = match request.turn_environment.executor_platform_os.as_deref() {
            Some("linux" | "macos") => DangerousCommandPlatform::Posix,
            Some("windows") | Some(_) => DangerousCommandPlatform::Windows,
            None => DangerousCommandPlatform::host(),
        };
        let exec_approval_requirement = context
            .session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_shell(
                ExecApprovalRequest {
                    command: &request.command,
                    approval_policy: context.step_context.settings.approval_policy(),
                    permission_profile: request.turn_environment.permission_profile().clone(),
                    environment_policy: request.turn_environment.config().exec_policy.as_ref(),
                    windows_sandbox_level: request.turn_environment.config().windows_sandbox_level,
                    sandbox_permissions: if request.additional_permissions_preapproved {
                        crate::sandboxing::SandboxPermissions::UseDefault
                    } else {
                        request.sandbox_permissions
                    },
                    prefix_rule: request.prefix_rule.clone(),
                    allow_prefix_rules: context.step_context.turn.allow_prefix_rules(),
                },
                configured_shell,
                &request.shell_mode,
                command_platform,
            )
            .await;
        let req = UnifiedExecToolRequest {
            command: request.command.clone(),
            shell_type: request.shell_type,
            hook_command: request.hook_command.clone(),
            process_id: request.process_id_lease.process_id(),
            cwd,
            sandbox_cwd: request.sandbox_cwd.clone(),
            turn_environment: request.turn_environment.clone(),
            env,
            exec_server_env_config: Some(exec_server_env_config),
            shell_snapshot,
            explicit_env_overrides,
            network: request.network.clone(),
            tty: request.tty,
            sandbox_permissions: request.sandbox_permissions,
            additional_permissions: request.additional_permissions.clone(),
            #[cfg(unix)]
            additional_permissions_preapproved: request.additional_permissions_preapproved,
            justification: request.justification.clone(),
            exec_approval_requirement,
        };
        let tool_ctx = ToolCtx {
            session: context.session.clone(),
            step_context: Arc::clone(&context.step_context),
            cancellation_token: context.cancellation_token.clone(),
            call_id: context.call_id.clone(),
            tool_name: ToolName::plain("exec_command"),
        };
        orchestrator
            .run(&mut runtime, &req, &tool_ctx)
            .await
            .map(|result| (result.output, result.deferred_network_approval))
            .map_err(|err| match err {
                ToolError::Codex(err) => match err.details() {
                    CodexErrorDetails::Sandbox(SandboxErr::Denied { output, .. }) => {
                        let output = output.as_ref().clone();
                        let message = if output.aggregated_output.text.is_empty() {
                            let exit_code = output.exit_code;
                            format!("Process exited with code {exit_code}")
                        } else {
                            output.aggregated_output.text.clone()
                        };
                        UnifiedExecError::sandbox_denied(message, output)
                    }
                    _ => UnifiedExecError::create_process(format!("{err:?}")),
                },
                other => UnifiedExecError::create_process(format!("{other:?}")),
            })
    }

    pub(super) async fn collect_output_until_deadline<const MAX_BYTES: usize>(
        output: &OutputHandles<MAX_BYTES>,
        mut pause_state: Option<watch::Receiver<bool>>,
        mut deadline: Instant,
    ) -> HeadTailBuffer<MAX_BYTES> {
        const POST_EXIT_CLOSE_WAIT_CAP: Duration = Duration::from_millis(50);

        let OutputHandles {
            output_buffer,
            transcript: _,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
        } = output;
        let mut collected = HeadTailBuffer::default();
        let mut exit_signal_received = cancellation_token.is_cancelled();
        let mut post_exit_deadline: Option<Instant> = None;
        loop {
            Self::extend_deadlines_while_paused(
                &mut pause_state,
                &mut deadline,
                &mut post_exit_deadline,
            )
            .await;
            let output_notified = output_notify.notified();
            let output_closed_notified = output_closed_notify.notified();
            tokio::pin!(output_notified);
            tokio::pin!(output_closed_notified);
            // Both producers use `notify_waiters`, which does not retain a
            // permit. Register before inspecting the buffer and close flag.
            output_notified.as_mut().enable();
            output_closed_notified.as_mut().enable();
            let drained_output: HeadTailBuffer<MAX_BYTES>;
            let has_drained_output: bool;
            {
                let mut guard = output_buffer.lock().await;
                drained_output = std::mem::take(&mut *guard);
                has_drained_output =
                    drained_output.retained_bytes() > 0 || drained_output.omitted_bytes() > 0;
            }

            if !has_drained_output {
                exit_signal_received |= cancellation_token.is_cancelled();
                if exit_signal_received && output_closed.load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    break;
                }

                if exit_signal_received {
                    let now = Instant::now();
                    let close_wait_deadline = *post_exit_deadline
                        .get_or_insert_with(|| now + remaining.min(POST_EXIT_CLOSE_WAIT_CAP));
                    let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
                    if close_wait_remaining == Duration::ZERO {
                        break;
                    }
                    tokio::select! {
                        _ = &mut output_notified => {}
                        _ = &mut output_closed_notified => {}
                        _ = tokio::time::sleep(close_wait_remaining) => {}
                        _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                    }
                    continue;
                }

                let exit_notified = cancellation_token.cancelled();
                tokio::pin!(exit_notified);
                tokio::select! {
                    _ = &mut output_notified => {}
                    _ = &mut exit_notified => exit_signal_received = true,
                    _ = tokio::time::sleep(remaining) => {}
                    _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                }
                continue;
            }

            collected.push_buffer(drained_output);

            exit_signal_received |= cancellation_token.is_cancelled();
            if Instant::now() >= deadline {
                break;
            }
        }

        collected
    }

    async fn extend_deadlines_while_paused(
        pause_state: &mut Option<watch::Receiver<bool>>,
        deadline: &mut Instant,
        post_exit_deadline: &mut Option<Instant>,
    ) {
        let Some(receiver) = pause_state.as_mut() else {
            return;
        };
        if !*receiver.borrow() {
            return;
        }

        let paused_at = Instant::now();
        while *receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }

        let paused_for = paused_at.elapsed();
        *deadline += paused_for;
        if let Some(post_exit_deadline) = post_exit_deadline.as_mut() {
            *post_exit_deadline += paused_for;
        }
    }

    async fn wait_for_pause_change(pause_state: Option<&watch::Receiver<bool>>) {
        match pause_state {
            Some(pause_state) => {
                let mut receiver = pause_state.clone();
                let _ = receiver.changed().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> Option<ProcessEntry> {
        if store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES {
            return None;
        }

        let mut meta: Vec<(i32, Instant, bool)> = store
            .processes
            .iter()
            .map(|(id, entry)| (*id, entry.last_used, entry.process.has_exited()))
            .collect();
        let mut found_locked_exited_process = false;

        while let Some(process_id) = Self::process_id_to_prune_from_meta(&meta) {
            let candidate_process = store
                .processes
                .get(&process_id)
                .map(|entry| Arc::clone(&entry.process));
            let candidate_has_exited = candidate_process
                .as_ref()
                .is_some_and(|process| process.has_exited());
            if found_locked_exited_process && !candidate_has_exited {
                // The store may temporarily exceed its soft cap while an exited
                // process is publishing its terminal event. Do not evict a live
                // process just because that exited process is briefly locked.
                return None;
            }

            let initial_exec_command_active = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| entry.initial_exec_command_active.load(Ordering::Acquire));
            if initial_exec_command_active {
                found_locked_exited_process |= candidate_has_exited;
                meta.retain(|(id, _, _)| *id != process_id);
                continue;
            }

            if candidate_process
                .as_ref()
                .is_some_and(|process| process.has_active_operation())
            {
                found_locked_exited_process |= candidate_has_exited;
                meta.retain(|(id, _, _)| *id != process_id);
                continue;
            }

            // Do not prune processes while an interaction, event-driven wait,
            // or terminal event publication is active for them.
            if let Some(candidate_process) = candidate_process.as_ref() {
                let interaction_lock = candidate_process.interaction_lock();
                let wait_process_lock = candidate_process.wait_process_lock();
                if let Ok(_interaction_guard) = interaction_lock.try_lock_owned()
                    && let Ok(_wait_process_guard) = wait_process_lock.try_lock_owned()
                {
                    let process_id_lease = store
                        .processes
                        .get(&process_id)
                        .map(|entry| entry.process_id_lease.clone());
                    return process_id_lease.as_ref().and_then(|process_id_lease| {
                        store.remove_if_process_matches(process_id_lease, candidate_process)
                    });
                }
            }
            found_locked_exited_process |= candidate_has_exited
                || candidate_process.is_some_and(|process| process.has_exited());
            meta.retain(|(id, _, _)| *id != process_id);
        }

        None
    }

    // Centralized pruning policy so we can easily swap strategies later.
    fn process_id_to_prune_from_meta(meta: &[(i32, Instant, bool)]) -> Option<i32> {
        if meta.is_empty() {
            return None;
        }

        let mut by_recency = meta.to_vec();
        by_recency.sort_by_key(|(_, last_used, _)| Reverse(*last_used));
        let protected: HashSet<i32> = by_recency
            .iter()
            .take(8)
            .map(|(process_id, _, _)| *process_id)
            .collect();

        let mut lru = meta.to_vec();
        lru.sort_by_key(|(_, last_used, _)| *last_used);

        if let Some((process_id, _, _)) = lru
            .iter()
            .find(|(process_id, _, exited)| !protected.contains(process_id) && *exited)
        {
            return Some(*process_id);
        }

        lru.into_iter()
            .find(|(process_id, _, _)| !protected.contains(process_id))
            .map(|(process_id, _, _)| process_id)
    }

    pub(crate) async fn terminate_all_processes(&self) {
        self.terminate_all_processes_for_scope(ProcessCleanupScope::Live)
            .await;
    }

    pub(crate) async fn terminate_all_processes_for_session_shutdown(&self) {
        self.terminate_all_processes_for_scope(ProcessCleanupScope::SessionShutdown)
            .await;
    }

    async fn terminate_all_processes_for_scope(&self, cleanup_scope: ProcessCleanupScope) {
        let (entries, pending_launches): (Vec<ProcessEntry>, Vec<ProcessIdLease>) = {
            let mut store = self.process_store.lock().await;
            let mut seen_pending_launches = HashSet::new();
            let reserved_process_ids = store.reserved_process_ids.snapshot();
            let pending_launches = reserved_process_ids
                .iter()
                .chain(
                    store
                        .processes
                        .values()
                        .map(|entry| &entry.process_id_lease),
                )
                .filter_map(|process_id_lease| {
                    let pending_launch = process_id_lease.pending_launch();
                    if seen_pending_launches.insert(Arc::as_ptr(pending_launch)) {
                        Some(process_id_lease.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for process_id_lease in &pending_launches {
                process_id_lease.pending_launch().request_cleanup();
            }
            let process_id_leases: Vec<ProcessIdLease> = store
                .processes
                .values()
                .map(|entry| entry.process_id_lease.clone())
                .collect();
            let mut entries = Vec::with_capacity(process_id_leases.len());
            for process_id_lease in &process_id_leases {
                let process_id = process_id_lease.process_id();
                let entry_matches = store
                    .processes
                    .get(&process_id)
                    .is_some_and(|entry| entry.process_id_lease.same_allocation(process_id_lease));
                if entry_matches && let Some(entry) = store.processes.remove(&process_id) {
                    entries.push(entry);
                }
            }
            if matches!(cleanup_scope, ProcessCleanupScope::SessionShutdown) {
                // Session shutdown invalidates pre-store allocations too. A
                // spawn finishing afterward must fail its identity-checked
                // store rather than resurrecting a terminal after shutdown.
                store.reserved_process_ids.clear();
            }
            (entries, pending_launches)
        };

        let cleanup = async {
            let stored_process_cleanup =
                futures::future::join_all(entries.iter().map(|entry| async {
                    if !entry.process.has_exited()
                        && let Err(error) = entry.process.terminate_confirmed().await
                    {
                        entry.process.mark_termination_failure(error.to_string());
                    }
                }));
            let pending_launch_cleanup = futures::future::join_all(
                pending_launches
                    .iter()
                    .map(|process_id_lease| process_id_lease.pending_launch().wait()),
            );
            let network_approval_cleanup =
                futures::future::join_all(entries.iter().map(|entry| async {
                    let session = entry.session.upgrade();
                    let _ = finish_deferred_network_approval_for_session(
                        session.as_ref(),
                        entry.network_approval.clone(),
                    )
                    .await;
                }));
            tokio::join!(
                stored_process_cleanup,
                pending_launch_cleanup,
                network_approval_cleanup
            );
        };

        match cleanup_scope {
            ProcessCleanupScope::Live => {
                cleanup.await;
                let mut store = self.process_store.lock().await;
                for process_id_lease in &pending_launches {
                    store.release_reservation(process_id_lease);
                }
            }
            ProcessCleanupScope::SessionShutdown => {
                if tokio::time::timeout(UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE, cleanup)
                    .await
                    .is_ok()
                {
                    return;
                }

                let unfinished_processes = entries
                    .iter()
                    .filter(|entry| !entry.process.has_exited())
                    .collect::<Vec<_>>();
                let unfinished_process_ids = unfinished_processes
                    .iter()
                    .map(|entry| entry.process_id)
                    .collect::<Vec<_>>();
                futures::future::join_all(entries.iter().map(|entry| {
                    entry
                        .process
                        .force_shutdown_cleanup(UNIFIED_EXEC_SHUTDOWN_CLEANUP_FAILURE)
                }))
                .await;
                let unfinished_pending_launch_ids = pending_launches
                    .iter()
                    .filter(|process_id_lease| !process_id_lease.pending_launch().is_complete())
                    .map(ProcessIdLease::process_id)
                    .collect::<Vec<_>>();
                tracing::warn!(
                    shutdown_cleanup_grace_ms = UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE.as_millis() as u64,
                    unfinished_process_count = unfinished_process_ids.len(),
                    unfinished_process_ids = ?unfinished_process_ids,
                    unfinished_pending_launch_count = unfinished_pending_launch_ids.len(),
                    unfinished_pending_launch_ids = ?unfinished_pending_launch_ids,
                    "unified exec session shutdown cleanup grace elapsed"
                );
                let outcome = super::pending_launch::PendingLaunchOutcome {
                    termination_error: Some(UNIFIED_EXEC_SHUTDOWN_CLEANUP_FAILURE.to_string()),
                    network_approval_error: None,
                };
                futures::future::join_all(pending_launches.iter().map(|process_id_lease| {
                    process_id_lease
                        .pending_launch()
                        .force_shutdown(outcome.clone())
                }))
                .await;
            }
        }
    }

    pub(crate) async fn list_processes(&self) -> Vec<BackgroundTerminalInfo> {
        let store = self.process_store.lock().await;
        let mut entries = store
            .processes
            .values()
            .filter(|entry| !entry.process.has_exited())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.process_id);
        entries
            .into_iter()
            .map(|entry| BackgroundTerminalInfo {
                item_id: entry.call_id.clone(),
                process_id: entry.process_id.to_string(),
                command: entry.hook_command.clone(),
                cwd: entry.cwd.clone(),
            })
            .collect()
    }

    pub(crate) async fn terminate_process(&self, process_id: i32) -> bool {
        let (process, process_id_lease, already_exited) = {
            let store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return false;
            };
            (
                Arc::clone(&entry.process),
                entry.process_id_lease.clone(),
                entry.process.has_exited(),
            )
        };

        if !already_exited && process.terminate_confirmed().await.is_err() {
            return false;
        }

        let entry = {
            let mut store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return true;
            };
            if !Arc::ptr_eq(&entry.process, &process) {
                return true;
            }
            if entry.initial_exec_command_active.load(Ordering::Acquire) {
                return true;
            }
            let Some(entry) = store.remove_if_process_matches(&process_id_lease, &process) else {
                return false;
            };
            entry
        };

        unregister_network_approval_for_entry(&entry).await;
        true
    }
}

enum ProcessStatus {
    Alive {
        exit_code: Option<i32>,
        call_id: String,
        process_id: i32,
    },
    Exited {
        exit_code: Option<i32>,
        entry: Box<ProcessEntry>,
    },
    Unknown,
}

#[cfg(test)]
#[path = "process_manager_tests.rs"]
mod tests;

mod wait_process;
