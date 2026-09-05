use super::*;
use crate::guardian::GuardianNetworkAccessTrigger;
use crate::tools::network_approval::NetworkApprovalSpec;
use crate::tools::network_approval::begin_network_approval;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::take_plugin_metrics_sidecar;
use codex_core_plugins::PluginMetricsOperation;
use codex_core_plugins::PluginMetricsSidecar;
use codex_core_plugins::ResolvedPluginMetricsOperation;
use codex_network_proxy::BlockedRequest;
use codex_network_proxy::BlockedRequestArgs;
use codex_network_proxy::ConfigReloader;
use codex_network_proxy::ConfigReloaderFuture;
use codex_network_proxy::ConfigState;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_network_proxy::NetworkProxyConfig;
use codex_network_proxy::NetworkProxyConstraints;
use codex_network_proxy::NetworkProxyState;
use codex_plugin::PluginId;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use tokio::sync::Barrier;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;

struct StaticNetworkProxyReloader;

impl ConfigReloader for StaticNetworkProxyReloader {
    fn source_label(&self) -> String {
        "unified-exec test config".to_string()
    }

    fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
        Box::pin(async { Err(anyhow::anyhow!("test config cannot be reloaded")) })
    }
}

async fn network_proxy_for_test() -> anyhow::Result<NetworkProxy> {
    let state = codex_network_proxy::build_config_state(
        NetworkProxyConfig::default(),
        NetworkProxyConstraints::default(),
    )?;
    NetworkProxy::builder()
        .state(Arc::new(NetworkProxyState::with_reloader(
            state,
            Arc::new(StaticNetworkProxyReloader),
        )))
        .managed_by_codex(/*managed_by_codex*/ false)
        .http_addr(([127, 0, 0, 1], 0).into())
        .socks_addr(([127, 0, 0, 1], 0).into())
        .build()
        .await
}

fn process_entry_for_lease(
    turn: &crate::session::turn_context::TurnContext,
    process_id_lease: crate::unified_exec::ProcessIdLease,
    process: Arc<crate::unified_exec::UnifiedExecProcess>,
) -> ProcessEntry {
    let process_id = process_id_lease.process_id();
    ProcessEntry {
        process,
        process_id_lease,
        call_id: format!("call-{process_id}"),
        process_id,
        cwd: PathUri::parse("file:///tmp").expect("test cwd should be valid"),
        initial_exec_command_active: Arc::new(AtomicBool::new(false)),
        hook_command: format!("command-{process_id}"),
        tty: false,
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        permissions: super::super::TerminalPermissions::for_launch(
            turn.environments.primary().expect("turn environment"),
            turn,
            super::super::TerminalSandboxSource::Native,
            crate::sandboxing::SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
            /*internal_permissions*/ None,
        ),
        network_approval: None,
        session: std::sync::Weak::new(),
        last_used: Instant::now(),
    }
}

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), "cat".to_string()),
        ("GIT_PAGER".to_string(), "cat".to_string()),
        ("GH_PAGER".to_string(), "cat".to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_overrides_existing_values() {
    let mut base = HashMap::new();
    base.insert("NO_COLOR".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base);

    assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[tokio::test]
async fn deterministic_process_ids_are_not_reused_after_release() {
    let manager = UnifiedExecProcessManager::default();
    let first_lease = manager.allocate_process_id().await;
    let first = first_lease.process_id();
    manager.release_process_id(&first_lease).await;
    let second = manager.allocate_process_id().await.process_id();

    assert_eq!((first, second), (1000, 1001));
}

#[tokio::test]
async fn cancelled_allocations_release_reservations_before_manager_handoff() {
    let manager = Arc::new(UnifiedExecProcessManager::default());

    for begin_launch in [false, true, false, true, false, true, false, true] {
        let manager_for_task = Arc::clone(&manager);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let launch = tokio::spawn(async move {
            let process_id_lease = manager_for_task.allocate_process_id().await;
            let _pending_launch_guard = if begin_launch {
                Some(
                    manager_for_task
                        .begin_pending_launch(&process_id_lease)
                        .await
                        .expect("reserved allocation should begin exactly once"),
                )
            } else {
                None
            };
            ready_tx
                .send(())
                .expect("test should still be waiting to cancel launch");
            std::future::pending::<()>().await;
        });
        ready_rx.await.expect("launch reached cancellation point");
        launch.abort();
        assert!(
            launch
                .await
                .expect_err("cancelled launch should not complete")
                .is_cancelled()
        );

        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }

    let next_lease = manager.allocate_process_id().await;
    assert_eq!(next_lease.process_id(), 1008);
    drop(next_lease);
    let store = manager.process_store.lock().await;
    assert!(store.processes.is_empty());
    assert!(store.reserved_process_ids.is_empty());
}

#[tokio::test]
async fn shutdown_cleanup_has_one_global_grace_for_stalled_stored_termination() -> anyhow::Result<()>
{
    tokio::time::pause();
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let (
        process,
        termination_count,
        termination_started,
        _termination_allowed,
        termination_call_dropped,
    ) = crate::unified_exec::process_tests::remote_process_with_gated_termination_drop_probe(
        codex_exec_server::WriteStatus::Accepted,
        codex_sandboxing::SandboxType::None,
    )
    .await;
    let process = Arc::new(process);
    process.attach_process_id_lease(process_id_lease.clone());
    let entry = process_entry_for_lease(&turn, process_id_lease.clone(), Arc::clone(&process));
    assert!(manager.process_store.lock().await.insert(entry).is_ok());

    let manager_for_shutdown = Arc::clone(&manager);
    let shutdown = tokio::spawn(async move {
        manager_for_shutdown
            .terminate_all_processes_for_session_shutdown()
            .await;
    });
    termination_started.notified().await;
    tokio::time::advance(Duration::from_secs(5) + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;

    assert!(
        shutdown.is_finished(),
        "explicit shutdown must stop waiting after one global cleanup grace"
    );
    shutdown.await?;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    assert!(
        termination_call_dropped.load(Ordering::Acquire),
        "forced shutdown must drop the detached remote termination future"
    );
    assert_eq!(
        process.failure_message().as_deref(),
        Some(UNIFIED_EXEC_SHUTDOWN_CLEANUP_FAILURE)
    );
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }

    tokio::time::resume();
    assert_eq!(process_id, 1000);
    Ok(())
}

#[tokio::test]
async fn shutdown_cleanup_finishes_normally_within_global_grace() -> anyhow::Result<()> {
    tokio::time::pause();
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process_id_lease = manager.allocate_process_id().await;
    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let (process, termination_count, termination_started, termination_allowed) =
        crate::unified_exec::process_tests::remote_process_with_gated_termination(
            codex_exec_server::WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    process.attach_process_id_lease(process_id_lease.clone());
    let entry = process_entry_for_lease(&turn, process_id_lease.clone(), Arc::clone(&process));
    assert!(manager.process_store.lock().await.insert(entry).is_ok());

    let manager_for_shutdown = Arc::clone(&manager);
    let shutdown = tokio::spawn(async move {
        manager_for_shutdown
            .terminate_all_processes_for_session_shutdown()
            .await;
    });
    termination_started.notified().await;
    tokio::time::advance(UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "shutdown should still wait for cleanup within its grace"
    );

    termination_allowed.notify_one();
    shutdown.await?;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    assert_eq!(process.failure_message(), None);
    assert!(pending_launch.is_complete());
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }
    tokio::time::resume();
    Ok(())
}

#[tokio::test]
async fn live_cleanup_waits_past_the_session_shutdown_grace() -> anyhow::Result<()> {
    tokio::time::pause();
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process_id_lease = manager.allocate_process_id().await;
    let (process, termination_count, termination_started, termination_allowed) =
        crate::unified_exec::process_tests::remote_process_with_gated_termination(
            codex_exec_server::WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    process.attach_process_id_lease(process_id_lease.clone());
    let entry = process_entry_for_lease(&turn, process_id_lease.clone(), Arc::clone(&process));
    assert!(manager.process_store.lock().await.insert(entry).is_ok());

    let manager_for_cleanup = Arc::clone(&manager);
    let mut cleanup = tokio::spawn(async move {
        manager_for_cleanup.terminate_all_processes().await;
    });
    termination_started.notified().await;
    tokio::time::advance(UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;

    assert!(
        !cleanup.is_finished(),
        "named live cleanup must retain unbounded completion semantics"
    );
    assert_eq!(process.failure_message(), None);
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reservation_matches(&process_id_lease),
        "live cleanup must retain ownership until termination completes"
    );

    termination_allowed.notify_one();
    (&mut cleanup).await?;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    assert!(process.has_exited());
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reserved_process_ids
            .is_empty()
    );
    tokio::time::resume();
    Ok(())
}

#[tokio::test]
async fn shutdown_cleanup_has_one_global_grace_for_stalled_pending_launch() -> anyhow::Result<()> {
    tokio::time::pause();
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process_id_lease = manager.allocate_process_id().await;
    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;

    let manager_for_shutdown = Arc::clone(&manager);
    let shutdown = tokio::spawn(async move {
        manager_for_shutdown
            .terminate_all_processes_for_session_shutdown()
            .await;
    });
    loop {
        if manager
            .process_store
            .lock()
            .await
            .reserved_process_ids
            .is_empty()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5) + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;

    assert!(
        shutdown.is_finished(),
        "explicit shutdown must stop waiting for an unpublished launch"
    );
    shutdown.await?;
    assert!(
        pending_launch.is_complete(),
        "forced shutdown must leave no unrecoverable pending launch after clearing the registry"
    );
    assert!(manager.process_store.lock().await.processes.is_empty());

    drop(pending_launch_guard);
    pending_launch.wait().await;
    drop(process_id_lease);
    tokio::time::resume();
    Ok(())
}

#[tokio::test]
async fn shutdown_force_drops_blocked_manager_owned_launch_and_releases_reservation()
-> anyhow::Result<()> {
    tokio::time::pause();
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process_id_lease = manager.allocate_process_id().await;
    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;
    let launch_started = Arc::new(Notify::new());
    let launch_started_for_task = Arc::clone(&launch_started);
    let launch_resource = Arc::new(());
    let launch_resource_probe = Arc::downgrade(&launch_resource);
    let launch_rx =
        pending_launch_guard.spawn_manager_owned(move |pending_launch_guard| async move {
            let _pending_launch_guard = pending_launch_guard;
            let _launch_resource = launch_resource;
            launch_started_for_task.notify_one();
            std::future::pending::<()>().await;
        });
    launch_started.notified().await;
    drop(launch_rx);

    let manager_for_shutdown = Arc::clone(&manager);
    let shutdown = tokio::spawn(async move {
        manager_for_shutdown
            .terminate_all_processes_for_session_shutdown()
            .await;
    });
    loop {
        if manager
            .process_store
            .lock()
            .await
            .reserved_process_ids
            .is_empty()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::advance(UNIFIED_EXEC_SHUTDOWN_CLEANUP_GRACE + Duration::from_millis(1)).await;
    shutdown.await?;

    assert!(pending_launch.is_complete());
    assert!(launch_resource_probe.upgrade().is_none());
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }
    drop(process_id_lease);
    tokio::time::resume();
    Ok(())
}

#[tokio::test]
async fn old_request_does_not_adopt_a_newer_reservation_for_the_same_numeric_id() {
    let manager = UnifiedExecProcessManager::default();
    let old_lease = manager.allocate_process_id().await;
    let process_id = old_lease.process_id();
    let original_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    original_process.attach_process_id_lease(old_lease.clone());
    let new_lease = crate::unified_exec::ProcessIdLease::new(process_id);
    {
        let store = manager.process_store.lock().await;
        store
            .reserved_process_ids
            .insert(process_id, new_lease.clone());
    }

    manager.remove_process_if_matches(&original_process).await;

    let store = manager.process_store.lock().await;
    assert!(store.reservation_matches(&new_lease));
    assert!(!store.reservation_matches(&old_lease));
}

#[tokio::test]
async fn initial_failure_removes_only_the_matching_stored_process() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = UnifiedExecProcessManager::default();
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let stored_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    stored_process.attach_process_id_lease(process_id_lease.clone());
    let stale_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    stale_process.attach_process_id_lease(process_id_lease.clone());
    let entry =
        process_entry_for_lease(&turn, process_id_lease.clone(), Arc::clone(&stored_process));
    assert!(manager.process_store.lock().await.insert(entry).is_ok());

    stored_process.fail_and_terminate("failure during initial yield".to_string());
    manager.remove_process_if_matches(&stale_process).await;
    {
        let store = manager.process_store.lock().await;
        let entry = store
            .processes
            .get(&process_id)
            .expect("stale cleanup must preserve the stored process");
        assert!(Arc::ptr_eq(&entry.process, &stored_process));
    }

    manager.remove_process_if_matches(&stored_process).await;
    let store = manager.process_store.lock().await;
    assert!(!store.processes.contains_key(&process_id));
    assert!(store.reservation_matches(&process_id_lease));
}

#[tokio::test]
async fn stale_store_attempt_does_not_overwrite_a_newer_process_entry() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process_id = 4242;
    let old_lease = crate::unified_exec::ProcessIdLease::new(process_id);
    let new_lease = crate::unified_exec::ProcessIdLease::new(process_id);
    let old_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let new_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let mut store = ProcessStore::default();
    store
        .reserved_process_ids
        .insert(process_id, new_lease.clone());
    let new_entry = process_entry_for_lease(&turn, new_lease.clone(), Arc::clone(&new_process));
    assert!(store.insert(new_entry).is_ok());

    let old_entry = process_entry_for_lease(&turn, old_lease, old_process);
    assert!(store.insert(old_entry).is_err());

    let stored = store
        .processes
        .get(&process_id)
        .expect("newer process entry should remain stored");
    assert!(stored.process_id_lease.same_allocation(&new_lease));
    assert!(Arc::ptr_eq(&stored.process, &new_process));
}

#[tokio::test]
async fn shutdown_waits_for_manager_owned_pending_launch_cleanup_after_origin_is_dropped()
-> anyhow::Result<()> {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;
    let (process, termination_count, termination_started, termination_allowed) =
        crate::unified_exec::process_tests::remote_process_with_gated_termination(
            codex_exec_server::WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    process.attach_process_id_lease(process_id_lease.clone());
    let call_id = "shutdown-during-spawn".to_string();
    let turn_environment = turn
        .environments
        .primary()
        .cloned()
        .expect("primary environment");
    let cwd = turn_environment.cwd().clone();
    let active_network_approval = begin_network_approval(
        &session,
        &turn,
        /*managed_network_active*/ true,
        Some(NetworkApprovalSpec {
            network: Some(network_proxy_for_test().await?),
            trigger: GuardianNetworkAccessTrigger {
                call_id: call_id.clone(),
                tool_name: "exec_command".to_string(),
                command: vec!["remote-command".to_string()],
                cwd: cwd.clone(),
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: None,
                tty: Some(false),
            },
            tool_name: ToolName::plain("exec_command"),
            command: "remote-command".to_string(),
            environment_id: turn_environment.selection.environment_id.clone(),
            permission_profile: turn_environment.permission_profile().clone(),
            network_policy: None,
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to begin network approval: {err:?}"))?
    .expect("managed network should create an active approval");
    let network_approval = active_network_approval
        .into_deferred()
        .expect("active approval should have a registration");
    let approval_probe = network_approval.clone();
    let registration_id = network_approval.registration_id().to_string();
    let (monitor_drop_tx, monitor_drop_rx) = tokio::sync::oneshot::channel::<()>();
    let network_denial_monitor = process_id_lease
        .pending_launch()
        .spawn_manager_task(async move {
            let _monitor_drop_tx = monitor_drop_tx;
            std::future::pending::<()>().await;
        });
    let metrics_sidecar = PluginMetricsSidecar::create(ResolvedPluginMetricsOperation {
        plugin_id: PluginId::parse("unified-exec-test@openai-curated")?,
        operation: PluginMetricsOperation {
            operation_name: "pending_launch_cleanup".to_string(),
            measurements: BTreeMap::new(),
        },
    })
    .expect("test metrics sidecar should be created");
    let plugin_metrics_sidecar = Arc::new(std::sync::Mutex::new(Some(metrics_sidecar)));
    let pending_decision = pending_launch_guard
        .launch()
        .publish()
        .expect("pending launch should publish exactly once");
    spawn_pending_launch_supervisor(
        process_id_lease.clone(),
        pending_decision,
        PendingLaunchResources {
            process: Arc::clone(&process),
            session: Arc::downgrade(&session),
            turn: Arc::clone(&turn),
            call_id: call_id.clone(),
            command: vec!["remote-command".to_string()],
            cwd: cwd.clone(),
            process_id,
            plugin_attribution: None,
            transcript: Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
            started_at: Instant::now(),
            network_approval: Some(network_approval),
            network_denial_monitor: Some(network_denial_monitor),
            plugin_metrics_sidecar: Some(Arc::clone(&plugin_metrics_sidecar)),
        },
    );

    // The originating tool future is paused after obtaining the process handle
    // but before it can store the process.
    let origin = tokio::spawn(async move {
        let _pending_launch_guard = pending_launch_guard;
        std::future::pending::<()>().await;
    });
    let shutdown_session = Arc::clone(&session);
    let mut shutdown = tokio::spawn(async move {
        shutdown_session
            .services
            .unified_exec_manager
            .terminate_all_processes_for_session_shutdown()
            .await;
    });

    termination_started.notified().await;
    assert!(!shutdown.is_finished());
    origin.abort();
    assert!(
        origin
            .await
            .expect_err("origin should be cancelled")
            .is_cancelled()
    );
    assert!(!shutdown.is_finished());
    termination_allowed.notify_one();
    (&mut shutdown).await?;

    assert!(process.has_exited());
    drop(process);
    tokio::task::yield_now().await;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), monitor_drop_rx)
            .await
            .expect("network-denial monitor should be stopped before store rejection returns")
            .is_err()
    );
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }
    assert!(take_plugin_metrics_sidecar(&plugin_metrics_sidecar).is_none());

    let mut blocked = BlockedRequest::new(BlockedRequestArgs {
        host: "cleanup.invalid".to_string(),
        reason: "not_allowed".to_string(),
        client: None,
        method: None,
        mode: None,
        protocol: "http".to_string(),
        decision: Some("deny".to_string()),
        source: Some("decider".to_string()),
        port: Some(80),
    });
    blocked.execution_id = Some(registration_id);
    session
        .services
        .network_approval
        .record_blocked_request(blocked)
        .await;
    tokio::task::yield_now().await;
    assert!(!approval_probe.is_cancelled());

    assert!(
        rx_event.try_recv().is_err(),
        "cleanup before the canonical start must not emit an unmatched terminal event"
    );
    Ok(())
}

#[tokio::test]
async fn oneshot_cancellation_before_process_publish_returns_and_preserves_reservation()
-> anyhow::Result<()> {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;
    let (process, termination_count, termination_started, termination_allowed) =
        crate::unified_exec::process_tests::remote_process_with_gated_termination(
            codex_exec_server::WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    let process_for_launch = Arc::clone(&process);
    let launch_started = Arc::new(Notify::new());
    let launch_allowed = Arc::new(Notify::new());
    let launch_started_for_task = Arc::clone(&launch_started);
    let launch_allowed_for_task = Arc::clone(&launch_allowed);
    let launch_session = Arc::clone(&session);
    let launch_turn = Arc::clone(&turn);
    let launch_lease = process_id_lease.clone();
    let cwd = turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd()
        .clone();
    let launch_rx =
        pending_launch_guard.spawn_manager_owned(move |pending_launch_guard| async move {
            launch_started_for_task.notify_one();
            launch_allowed_for_task.notified().await;
            process_for_launch.attach_process_id_lease(launch_lease.clone());
            let pending_decision = pending_launch_guard
                .launch()
                .publish()
                .expect("backend completion should publish cleanup ownership");
            spawn_pending_launch_supervisor(
                launch_lease,
                pending_decision,
                PendingLaunchResources {
                    process: process_for_launch,
                    session: Arc::downgrade(&launch_session),
                    turn: launch_turn,
                    call_id: "shutdown-during-backend-start".to_string(),
                    command: vec!["remote-command".to_string()],
                    cwd,
                    process_id,
                    plugin_attribution: None,
                    transcript: Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
                    started_at: Instant::now(),
                    network_approval: None,
                    network_denial_monitor: None,
                    plugin_metrics_sidecar: None,
                },
            );
            pending_launch_guard
        });
    launch_started.notified().await;
    drop(launch_rx);
    let cancellation_lease = process_id_lease.clone();
    let mut cancellation = tokio::spawn(async move {
        crate::unified_exec::oneshot::request_cancelled_execution_cleanup(&cancellation_lease);
    });
    tokio::task::yield_now().await;
    assert!(
        cancellation.is_finished(),
        "one-shot cancellation must not wait for a blocked backend start"
    );
    (&mut cancellation).await?;
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reservation_matches(&process_id_lease)
    );
    manager
        .process_store
        .lock()
        .await
        .next_deterministic_process_id = process_id;
    let next_lease = manager.allocate_process_id().await;
    assert_ne!(next_lease.process_id(), process_id);
    drop(next_lease);

    launch_allowed.notify_one();
    termination_started.notified().await;
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reservation_matches(&process_id_lease)
    );
    termination_allowed.notify_one();
    process_id_lease.pending_launch().wait().await;

    assert!(process.has_exited());
    drop(process);
    tokio::task::yield_now().await;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }
    assert!(
        rx_event.try_recv().is_err(),
        "cancellation before the canonical start must not emit an unmatched terminal event"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_and_origin_drop_during_begin_emit_one_paired_lifecycle() -> anyhow::Result<()> {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;
    let (process, termination_count, termination_started, termination_allowed) =
        crate::unified_exec::process_tests::remote_process_with_gated_termination(
            codex_exec_server::WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    process.attach_process_id_lease(process_id_lease.clone());
    let call_id = "cleanup-during-begin".to_string();
    let command = vec!["remote-command".to_string()];
    let cwd = turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd()
        .clone();
    let pending_decision = pending_launch_guard
        .launch()
        .publish()
        .expect("pending launch should publish exactly once");
    spawn_pending_launch_supervisor(
        process_id_lease.clone(),
        pending_decision,
        PendingLaunchResources {
            process: Arc::clone(&process),
            session: Arc::downgrade(&session),
            turn: Arc::clone(&turn),
            call_id: call_id.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
            process_id,
            plugin_attribution: None,
            transcript: Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
            started_at: Instant::now(),
            network_approval: None,
            network_denial_monitor: None,
            plugin_metrics_sidecar: None,
        },
    );

    let begin_started = Arc::new(Notify::new());
    let begin_allowed = Arc::new(Notify::new());
    let begin_started_for_task = Arc::clone(&begin_started);
    let begin_allowed_for_task = Arc::clone(&begin_allowed);
    let begin_session = Arc::clone(&session);
    let begin_turn = Arc::clone(&turn);
    let begin_call_id = call_id.clone();
    let begin_emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
        Some(process_id.to_string()),
        /*plugin_attribution*/ None,
    );
    let begin_rx = pending_launch_guard
        .spawn_manager_owned_begin(async move {
            begin_started_for_task.notify_one();
            begin_allowed_for_task.notified().await;
            begin_emitter
                .emit(
                    ToolEventCtx::new(
                        begin_session.as_ref(),
                        begin_turn.as_ref(),
                        &begin_call_id,
                        /*turn_diff_tracker*/ None,
                    ),
                    ToolEventStage::Begin,
                )
                .await;
        })
        .expect("start event should be admitted");
    begin_started.notified().await;

    let race_barrier = Arc::new(Barrier::new(3));
    let cleanup_launch = Arc::clone(process_id_lease.pending_launch());
    let cleanup_barrier = Arc::clone(&race_barrier);
    let cleanup = tokio::spawn(async move {
        cleanup_barrier.wait().await;
        cleanup_launch.request_cleanup();
    });
    let abandon_barrier = Arc::clone(&race_barrier);
    let abandon = tokio::spawn(async move {
        abandon_barrier.wait().await;
        drop(pending_launch_guard);
    });
    race_barrier.wait().await;
    cleanup.await?;
    abandon.await?;
    assert_eq!(termination_count.load(Ordering::Relaxed), 0);

    begin_allowed.notify_one();
    begin_rx
        .await
        .expect("manager-owned start event task should return")
        .expect("start event should commit");
    termination_started.notified().await;
    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let mut launch_completed = tokio::spawn(async move {
        pending_launch.wait().await;
    });
    termination_allowed.notify_one();
    tokio::time::timeout(Duration::from_millis(25), &mut launch_completed)
        .await
        .expect_err("terminal event must wait until streaming output is drained");
    session
        .send_event(
            turn.as_ref(),
            EventMsg::ExecCommandOutputDelta(
                codex_protocol::protocol::ExecCommandOutputDeltaEvent {
                    call_id: call_id.clone(),
                    stream: codex_protocol::protocol::ExecOutputStream::Stdout,
                    chunk: b"output-after-begin".to_vec(),
                },
            ),
        )
        .await;
    process.output_drained_notify().notify_one();
    launch_completed.await?;

    assert!(process.has_exited());
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.is_empty());
        assert!(store.reserved_process_ids.is_empty());
    }

    let started = rx_event.recv().await.expect("item started event");
    let codex_protocol::protocol::EventMsg::ItemStarted(started) = started.msg else {
        panic!("expected ItemStarted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(started) = started.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(
        (started.id, started.status, started.process_id),
        (
            call_id.clone(),
            codex_protocol::items::CommandExecutionStatus::InProgress,
            Some(process_id.to_string()),
        )
    );

    let legacy_begin = rx_event.recv().await.expect("legacy begin event");
    let codex_protocol::protocol::EventMsg::ExecCommandBegin(legacy_begin) = legacy_begin.msg
    else {
        panic!("expected ExecCommandBegin event");
    };
    assert_eq!(legacy_begin.call_id, call_id);

    let output_delta = rx_event.recv().await.expect("output delta event");
    let codex_protocol::protocol::EventMsg::ExecCommandOutputDelta(output_delta) = output_delta.msg
    else {
        panic!("expected ExecCommandOutputDelta event");
    };
    assert_eq!(
        output_delta,
        codex_protocol::protocol::ExecCommandOutputDeltaEvent {
            call_id: "cleanup-during-begin".to_string(),
            stream: codex_protocol::protocol::ExecOutputStream::Stdout,
            chunk: b"output-after-begin".to_vec(),
        }
    );

    let completed = rx_event.recv().await.expect("item completed event");
    let codex_protocol::protocol::EventMsg::ItemCompleted(completed) = completed.msg else {
        panic!("expected ItemCompleted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(completed) = completed.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(
        (completed.id, completed.status, completed.process_id),
        (
            "cleanup-during-begin".to_string(),
            codex_protocol::items::CommandExecutionStatus::Failed,
            Some(process_id.to_string()),
        )
    );

    let legacy_end = rx_event.recv().await.expect("legacy end event");
    let codex_protocol::protocol::EventMsg::ExecCommandEnd(legacy_end) = legacy_end.msg else {
        panic!("expected ExecCommandEnd event");
    };
    assert_eq!(
        (legacy_end.call_id, legacy_end.status),
        (
            "cleanup-during-begin".to_string(),
            codex_protocol::protocol::ExecCommandStatus::Failed,
        )
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn shutdown_waits_for_supervisor_terminal_side_effects_after_origin_is_dropped()
-> anyhow::Result<()> {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let manager = &session.services.unified_exec_manager;
    let process_id_lease = manager.allocate_process_id().await;
    let process_id = process_id_lease.process_id();
    let mut pending_launch_guard = manager.begin_pending_launch(&process_id_lease).await?;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    process.attach_process_id_lease(process_id_lease.clone());
    process.signal_exit(Some(0));
    let call_id = "short-process-origin-finish".to_string();
    let command = vec!["short-command".to_string()];
    let turn_environment = turn
        .environments
        .primary()
        .cloned()
        .expect("primary environment");
    let cwd = turn_environment.cwd().clone();
    let active_network_approval = begin_network_approval(
        &session,
        &turn,
        /*managed_network_active*/ true,
        Some(NetworkApprovalSpec {
            network: Some(network_proxy_for_test().await?),
            trigger: GuardianNetworkAccessTrigger {
                call_id: call_id.clone(),
                tool_name: "exec_command".to_string(),
                command: command.clone(),
                cwd: cwd.clone(),
                sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                additional_permissions: None,
                justification: None,
                tty: Some(false),
            },
            tool_name: ToolName::plain("exec_command"),
            command: "short-command".to_string(),
            environment_id: turn_environment.selection.environment_id.clone(),
            permission_profile: turn_environment.permission_profile().clone(),
            network_policy: None,
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to begin network approval: {err:?}"))?
    .expect("managed network should create an active approval");
    let network_approval = active_network_approval
        .into_deferred()
        .expect("active approval should have a registration");
    let approval_probe = network_approval.clone();
    let registration_id = network_approval.registration_id().to_string();
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript.lock().await.push_chunk(b"finished");
    let metrics_sidecar = PluginMetricsSidecar::create(ResolvedPluginMetricsOperation {
        plugin_id: PluginId::parse("unified-exec-test@openai-curated")?,
        operation: PluginMetricsOperation {
            operation_name: "short_process_terminal".to_string(),
            measurements: BTreeMap::new(),
        },
    })
    .expect("test metrics sidecar should be created");
    let plugin_metrics_sidecar = Arc::new(std::sync::Mutex::new(Some(metrics_sidecar)));
    let pending_decision = pending_launch_guard
        .launch()
        .publish()
        .expect("pending launch should publish exactly once");
    spawn_pending_launch_supervisor(
        process_id_lease.clone(),
        pending_decision,
        PendingLaunchResources {
            process: Arc::clone(&process),
            session: Arc::downgrade(&session),
            turn: Arc::clone(&turn),
            call_id: call_id.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
            process_id,
            plugin_attribution: None,
            transcript: Arc::clone(&transcript),
            started_at: Instant::now(),
            network_approval: Some(network_approval),
            network_denial_monitor: None,
            plugin_metrics_sidecar: Some(Arc::clone(&plugin_metrics_sidecar)),
        },
    );
    let begin_session = Arc::clone(&session);
    let begin_turn = Arc::clone(&turn);
    let begin_call_id = call_id.clone();
    let begin_emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
        Some(process_id.to_string()),
        /*plugin_attribution*/ None,
    );
    pending_launch_guard
        .spawn_manager_owned_begin(async move {
            begin_emitter
                .emit(
                    ToolEventCtx::new(
                        begin_session.as_ref(),
                        begin_turn.as_ref(),
                        &begin_call_id,
                        /*turn_diff_tracker*/ None,
                    ),
                    ToolEventStage::Begin,
                )
                .await;
        })
        .expect("start event should be admitted")
        .await
        .expect("manager-owned start event task should return")
        .expect("start event should commit");
    pending_launch_guard
        .watch_process()
        .expect("short process should transfer all terminal work to its supervisor");

    let pending_launch = Arc::clone(process_id_lease.pending_launch());
    let origin = tokio::spawn(async move {
        pending_launch.wait().await;
    });

    tokio::time::pause();
    let shutdown_session = Arc::clone(&session);
    let mut shutdown = tokio::spawn(async move {
        shutdown_session
            .services
            .unified_exec_manager
            .terminate_all_processes_for_session_shutdown()
            .await;
    });
    tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
        .await
        .expect_err("shutdown must wait for supervisor-owned terminal side effects");
    tokio::time::resume();

    origin.abort();
    assert!(
        origin
            .await
            .expect_err("origin should be cancelled")
            .is_cancelled()
    );
    process.fail_and_terminate("failure during terminal grace".to_string());
    process.output_drained_notify().notify_one();
    shutdown.await?;
    assert!(take_plugin_metrics_sidecar(&plugin_metrics_sidecar).is_none());

    let mut blocked = BlockedRequest::new(BlockedRequestArgs {
        host: "terminal-cleanup.invalid".to_string(),
        reason: "not_allowed".to_string(),
        client: None,
        method: None,
        mode: None,
        protocol: "http".to_string(),
        decision: Some("deny".to_string()),
        source: Some("decider".to_string()),
        port: Some(80),
    });
    blocked.execution_id = Some(registration_id);
    session
        .services
        .network_approval
        .record_blocked_request(blocked)
        .await;
    tokio::task::yield_now().await;
    assert!(!approval_probe.is_cancelled());

    let event = rx_event.recv().await.expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ItemStarted(started_event) = event.msg else {
        panic!("expected ItemStarted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(item) = started_event.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(
        (item.id, item.status, item.process_id),
        (
            "short-process-origin-finish".to_string(),
            codex_protocol::items::CommandExecutionStatus::InProgress,
            Some(process_id.to_string()),
        )
    );
    let legacy_event = rx_event.recv().await.expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ExecCommandBegin(legacy_event) = legacy_event.msg
    else {
        panic!("expected legacy ExecCommandBegin event");
    };
    assert_eq!(legacy_event.call_id, "short-process-origin-finish");

    let event = rx_event.recv().await.expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ItemCompleted(completed_event) = event.msg else {
        panic!("expected ItemCompleted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(item) = completed_event.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(
        (item.id, item.status, item.process_id),
        (
            "short-process-origin-finish".to_string(),
            codex_protocol::items::CommandExecutionStatus::Failed,
            Some(process_id.to_string()),
        )
    );
    let legacy_event = rx_event.recv().await.expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ExecCommandEnd(legacy_event) = legacy_event.msg else {
        panic!("expected legacy ExecCommandEnd event");
    };
    assert_eq!(
        (legacy_event.call_id, legacy_event.status),
        (
            "short-process-origin-finish".to_string(),
            codex_protocol::protocol::ExecCommandStatus::Failed,
        )
    );
    assert!(rx_event.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn stale_unconditional_cleanup_preserves_newer_reservation_and_process() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = UnifiedExecProcessManager::default();
    let old_lease = manager.allocate_process_id().await;
    let process_id = old_lease.process_id();
    let new_lease = crate::unified_exec::ProcessIdLease::new(process_id);
    let new_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    {
        let mut store = manager.process_store.lock().await;
        store
            .reserved_process_ids
            .insert(process_id, new_lease.clone());
        assert!(
            store
                .insert(process_entry_for_lease(
                    &turn,
                    new_lease.clone(),
                    Arc::clone(&new_process),
                ))
                .is_ok()
        );
    }

    manager.release_process_id(&old_lease).await;

    let store = manager.process_store.lock().await;
    assert!(store.reservation_matches(&new_lease));
    let stored = store
        .processes
        .get(&process_id)
        .expect("stale cleanup must preserve the newer process");
    assert!(stored.process_id_lease.same_allocation(&new_lease));
    assert!(Arc::ptr_eq(&stored.process, &new_process));
}

#[test]
fn env_overlay_for_exec_server_keeps_runtime_changes_only() {
    let local_policy_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/client-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        (
            CODEX_VERSION_ENV_VAR.to_string(),
            "client-version".to_string(),
        ),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
        (
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
            "1".to_string(),
        ),
    ]);
    let request_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/sandbox-path".to_string()),
        ("OpenAI_Federation_Rule_Id".to_string(), "rule".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        (
            CODEX_VERSION_ENV_VAR.to_string(),
            "client-version".to_string(),
        ),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
        (
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
            "1".to_string(),
        ),
        (
            "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
            "1".to_string(),
        ),
    ]);

    assert_eq!(
        env_overlay_for_exec_server(&request_env, &local_policy_env),
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                CODEX_VERSION_ENV_VAR.to_string(),
                "client-version".to_string()
            ),
            (
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                "current-profile".to_string(),
            ),
            (
                codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
                "1".to_string(),
            ),
            (
                "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
                "1".to_string()
            ),
        ])
    );
}

#[test]
fn exec_env_policy_excludes_non_inheritable_and_runtime_variables() {
    let policy = ShellEnvironmentPolicy {
        r#set: HashMap::from([
            ("codex_version".to_string(), "stale-version".to_string()),
            (
                "codex_permission_profile".to_string(),
                "stale-profile".to_string(),
            ),
            (
                "openai_identity_token_file".to_string(),
                "/run/identity-token".to_string(),
            ),
            (
                "codex_apply_patch_preserve_line_endings".to_string(),
                "1".to_string(),
            ),
            (
                "codex_plugin_metrics_output".to_string(),
                "/stale/sidecar".to_string(),
            ),
            ("KEEP".to_string(), "value".to_string()),
        ]),
        ..Default::default()
    };

    assert_eq!(
        exec_env_policy_from_shell_policy(&policy),
        codex_exec_server::ExecEnvPolicy {
            inherit: policy.inherit,
            ignore_default_excludes: policy.ignore_default_excludes,
            exclude: vec![
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                CODEX_VERSION_ENV_VAR.to_string(),
                codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
                PLUGIN_METRICS_OUTPUT_ENV_VAR.to_string(),
            ],
            r#set: HashMap::from([("KEEP".to_string(), "value".to_string())]),
            include_only: Vec::new(),
        }
    );
}

#[test]
fn exec_server_params_use_path_uri_and_env_policy_overlay_contract() {
    let cwd: codex_utils_absolute_path::AbsolutePathBuf = std::env::current_dir()
        .expect("current dir")
        .try_into()
        .expect("absolute path");
    let permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let managed_network = ManagedNetworkSandboxContext {
        loopback_ports: vec![43123],
        allow_local_binding: false,
    };
    let mut request = ExecRequest {
        command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
        cwd: cwd.clone().into(),
        env: HashMap::from([
            ("HOME".to_string(), "/client-home".to_string()),
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
            (
                "SSL_CERT_FILE".to_string(),
                "/client/custom-ca.pem".to_string(),
            ),
        ]),
        exec_server_env_config: Some(ExecServerEnvConfig {
            policy: codex_exec_server::ExecEnvPolicy {
                inherit: codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core,
                ignore_default_excludes: false,
                exclude: Vec::new(),
                r#set: HashMap::new(),
                include_only: Vec::new(),
            },
            local_policy_env: HashMap::from([
                ("HOME".to_string(), "/client-home".to_string()),
                ("PATH".to_string(), "/client-path".to_string()),
                (
                    "HTTP_PROXY".to_string(),
                    "http://127.0.0.1:43123".to_string(),
                ),
                ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
                (
                    "SSL_CERT_FILE".to_string(),
                    "/client/custom-ca.pem".to_string(),
                ),
            ]),
        }),
        exec_server_shell_snapshot: None,
        network: None,
        network_environment_id: None,
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
        sandbox: codex_sandboxing::SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: vec![cwd],
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        permission_profile: permission_profile.clone(),
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: true,
        exec_server_managed_network: Some(managed_network.clone()),
        exec_server_network_proxy: None,
    };

    let proxy_settings_mode = codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve;
    let params_for_request = |request: &ExecRequest| {
        exec_server_params_for_request(
            /*process_id*/ 123,
            request,
            /*tool_ctx*/ None,
            proxy_settings_mode,
            /*tty*/ true,
        )
    };
    let params = params_for_request(&request);

    assert_eq!(params.process_id.as_str(), "123");
    assert_eq!(params.metadata, None);
    assert_eq!(params.cwd, request.cwd);
    assert!(params.enforce_managed_network);
    assert_eq!(params.managed_network, Some(managed_network));
    assert!(params.env_policy.is_some());
    assert_eq!(
        params.env,
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string(),),
        ])
    );
    request.exec_server_shell_snapshot = Some(codex_exec_server::ShellSnapshotRequest {
        scope_id: "attachment-1".to_string(),
        shell: codex_exec_server::ShellInfo {
            name: "bash".to_string(),
            path: "/bin/bash".to_string(),
        },
    });
    let mut snapshot_env = params.env;
    snapshot_env.remove("PATH");
    assert_eq!(params_for_request(&request).env, snapshot_env);
    request.exec_server_shell_snapshot = None;

    request.exec_server_sandbox = Some(
        codex_exec_server::FileSystemSandboxContext::from_permission_profile(permission_profile),
    );
    let first = params_for_request(&request);
    let second = params_for_request(&request);
    assert_eq!(
        first
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.windows_sandbox_proxy_settings_mode),
        Some(codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve)
    );
    assert!(first.process_id.as_str().starts_with("123-"));
    assert!(second.process_id.as_str().starts_with("123-"));
    assert_ne!(first.process_id, second.process_id);
}

#[cfg(windows)]
#[test]
fn initial_exec_yield_time_uses_windows_floor() {
    let above_max_yield_time_ms = crate::unified_exec::MAX_YIELD_TIME_MS + 1;

    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 2_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 5_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 10_000), 10_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ above_max_yield_time_ms),
        crate::unified_exec::MAX_YIELD_TIME_MS
    );
}

#[cfg(not(windows))]
#[test]
fn initial_exec_yield_time_has_no_platform_floor() {
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 1_000), 1_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
}

#[tokio::test]
async fn output_collection_stays_bounded_across_repeated_drains() {
    let chunks: [&[u8]; 4] = [b"01234567", b"89ABCDEF", b"ghijklmnopq", b"rs"];
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::<10>::default()));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let output = OutputHandles {
        output_buffer: Arc::clone(&output_buffer),
        transcript: Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
        output_notify: Arc::clone(&output_notify),
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::clone(&output_closed_notify),
        cancellation_token: cancellation_token.clone(),
    };

    let collect = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(5),
    );
    let produce = async {
        for chunk in chunks {
            output_buffer.lock().await.push_chunk(chunk);
            output_notify.notify_waiters();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if output_buffer.lock().await.retained_bytes() == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("collector should drain each chunk");
        }

        output_closed.store(true, Ordering::Release);
        cancellation_token.cancel();
        output_closed_notify.notify_waiters();
        output_notify.notify_waiters();
    };

    let (collected, ()) = tokio::join!(collect, produce);
    let mut expected = HeadTailBuffer::<10>::default();
    for chunk in chunks {
        expected.push_chunk(chunk);
    }
    assert_eq!(collected, expected);
}

#[tokio::test]
async fn output_collection_preserves_omissions_from_drained_buffer() {
    let mut buffered_output = HeadTailBuffer::<10>::default();
    buffered_output.push_chunk(&[b'a'; 10]);
    buffered_output.push_chunk(b"overflow");
    let mut expected = HeadTailBuffer::<10>::default();
    expected.push_chunk(&[b'a'; 10]);
    expected.push_chunk(b"overflow");
    let output_buffer = Arc::new(tokio::sync::Mutex::new(buffered_output));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let output = OutputHandles {
        output_buffer,
        transcript: Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
    };

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(1),
    )
    .await;

    assert_eq!(collected, expected);
}

#[tokio::test]
async fn network_denial_fallback_message_names_sandbox_network_proxy() {
    let message = network_denial_message_for_session(/*session*/ None, /*deferred*/ None).await;

    assert_eq!(
        message,
        "Network access was denied by the Codex sandbox network proxy."
    );
}

#[tokio::test]
async fn initial_network_denial_waits_for_resolved_detail_before_failing_process() {
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let resolution_started = Arc::new(Notify::new());
    let resolution_allowed = Arc::new(Notify::new());
    let resolution_started_for_task = Arc::clone(&resolution_started);
    let resolution_allowed_for_task = Arc::clone(&resolution_allowed);
    let process_for_task = Arc::clone(&process);
    let mut failure = tokio::spawn(async move {
        fail_process_after_message_resolution(process_for_task.as_ref(), async move {
            resolution_started_for_task.notify_one();
            resolution_allowed_for_task.notified().await;
            "Network request to https://denied.example:443 was denied".to_string()
        })
        .await
    });

    resolution_started.notified().await;
    tokio::task::yield_now().await;
    assert!(!failure.is_finished());
    assert_eq!(process.failure_message(), None);

    resolution_allowed.notify_one();
    let _ = (&mut failure)
        .await
        .expect("network denial task should finish");
    assert_eq!(
        process.failure_message().as_deref(),
        Some("Network request to https://denied.example:443 was denied")
    );
}

#[tokio::test]
async fn late_network_denial_grace_observes_cancellation_after_exit() {
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation_for_task.cancel();
    });

    assert!(wait_for_late_network_denial(Some(cancellation)).await);
}

#[tokio::test]
async fn failed_initial_end_for_unstored_process_uses_fallback_output() {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        tokio_util::sync::CancellationToken::new(),
        "call-unified-denied".to_string(),
    );
    let request = ExecCommandRequest {
        command: vec![
            "sh".to_string(),
            "-lc".to_string(),
            "echo before".to_string(),
        ],
        shell_type: crate::shell::ShellType::Sh,
        hook_command: "echo before".to_string(),
        process_id_lease: crate::unified_exec::ProcessIdLease::new(123),
        yield_time_ms: 1000,
        max_output_tokens: None,
        #[allow(deprecated)]
        cwd: turn.cwd.clone().into(),
        #[allow(deprecated)]
        sandbox_cwd: turn.cwd.clone().into(),
        turn_environment: turn
            .environments
            .primary()
            .cloned()
            .expect("primary environment"),
        shell_mode: codex_tools::UnifiedExecShellMode::Direct,
        network: None,
        tty: true,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
    };

    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript.lock().await.push_chunk(b"PARTIAL_TRANSCRIPT");

    emit_failed_initial_exec_end_if_unstored(
        /*process_started_alive*/ false,
        &context,
        &request,
        #[allow(deprecated)]
        turn.cwd.clone().into(),
        /*plugin_attribution*/ None,
        transcript,
        "PRE_DENIAL_MARKER".to_string(),
        "Network access denied".to_string(),
        Duration::from_millis(7),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("timed out waiting for failed command execution item")
        .expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ItemCompleted(completed_event) = event.msg else {
        panic!("expected ItemCompleted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(item) = completed_event.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(item.id, "call-unified-denied");
    assert_eq!(
        item.status,
        codex_protocol::items::CommandExecutionStatus::Failed
    );
    assert_eq!(item.exit_code, Some(-1));
    assert_eq!(item.process_id.as_deref(), Some("123"));
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("PRE_DENIAL_MARKER\nNetwork access denied")
    );
}

#[test]
fn pruning_prefers_exited_processes_outside_recently_used() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), true),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(2));
}

#[test]
fn pruning_falls_back_to_lru_when_no_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(1));
}

#[test]
fn pruning_protects_recent_processes_even_if_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), true),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), true),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    // (10) is exited but among the last 8; we should drop the LRU outside that set.
    assert_eq!(candidate, Some(1));
}

#[tokio::test]
async fn pruned_process_retains_allocation_until_supervisor_finalizes() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let manager = UnifiedExecProcessManager::default();
    let pruned_lease = crate::unified_exec::ProcessIdLease::new(1);
    let pruned_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    pruned_process.attach_process_id_lease(pruned_lease.clone());
    pruned_process
        .terminate_confirmed()
        .await
        .expect("prune candidate should be exited");
    let live_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let now = Instant::now();
    {
        let mut store = manager.process_store.lock().await;
        let max_process_id =
            i32::try_from(MAX_UNIFIED_EXEC_PROCESSES).expect("process cap should fit in i32");
        for process_id in 1..=max_process_id {
            let process_id_lease = if process_id == 1 {
                pruned_lease.clone()
            } else {
                crate::unified_exec::ProcessIdLease::new(process_id)
            };
            store
                .reserved_process_ids
                .insert(process_id, process_id_lease.clone());
            let mut entry = process_entry_for_lease(
                &turn,
                process_id_lease,
                if process_id == 1 {
                    Arc::clone(&pruned_process)
                } else {
                    Arc::clone(&live_process)
                },
            );
            entry.last_used = if process_id == 1 {
                now - Duration::from_secs(1)
            } else {
                now + Duration::from_millis(
                    u64::try_from(process_id).expect("positive process id should fit u64"),
                )
            };
            assert!(store.insert(entry).is_ok());
        }

        let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store)
            .expect("oldest exited process should be pruned");
        assert_eq!(pruned.process_id, 1);
        assert!(store.reservation_matches(&pruned_lease));
        store.next_deterministic_process_id = 1;
    }

    // A stale caller that only still holds the removed process must not release
    // its allocation before the lifecycle supervisor has finished.
    manager.remove_process_if_matches(&pruned_process).await;
    let replacement = manager.allocate_process_id().await;

    assert_eq!(replacement.process_id(), 65);
    let store = manager.process_store.lock().await;
    assert!(store.reservation_matches(&pruned_lease));
}

#[cfg(unix)]
#[tokio::test]
async fn pruning_preserves_processes_in_use_or_with_active_initial_calls() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let exited_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    exited_process
        .terminate_confirmed()
        .await
        .expect("exited process should terminate");
    let live_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let interaction_guard = exited_process.interaction_lock().lock_owned().await;
    let now = Instant::now();
    let cwd = PathUri::parse("file:///tmp").expect("test cwd should be valid");
    let mut store = ProcessStore::default();
    let max_process_id =
        i32::try_from(MAX_UNIFIED_EXEC_PROCESSES).expect("process cap should fit in i32");

    for process_id in 1..=max_process_id {
        let is_exited = process_id == 1;
        let process_id_lease = crate::unified_exec::ProcessIdLease::new(process_id);
        store
            .reserved_process_ids
            .insert(process_id, process_id_lease.clone());
        store.processes.insert(
            process_id,
            ProcessEntry {
                process: if is_exited {
                    Arc::clone(&exited_process)
                } else {
                    Arc::clone(&live_process)
                },
                process_id_lease,
                call_id: format!("call-{process_id}"),
                process_id,
                cwd: cwd.clone(),
                initial_exec_command_active: Arc::new(AtomicBool::new(false)),
                hook_command: format!("command-{process_id}"),
                tty: false,
                environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
                permissions: super::super::TerminalPermissions::for_launch(
                    turn.environments.primary().expect("turn environment"),
                    &turn,
                    super::super::TerminalSandboxSource::Native,
                    crate::sandboxing::SandboxPermissions::UseDefault,
                    /*additional_permissions*/ None,
                    /*internal_permissions*/ None,
                ),
                network_approval: None,
                session: std::sync::Weak::new(),
                last_used: if is_exited {
                    now - Duration::from_secs(1)
                } else {
                    now
                },
            },
        );
    }
    let manager = UnifiedExecProcessManager::default();
    *manager.process_store.lock().await = store;

    let pruned = {
        let mut store = manager.process_store.lock().await;
        let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);
        (pruned.map(|entry| entry.process_id), store.processes.len())
    };

    assert_eq!(pruned, (None, MAX_UNIFIED_EXEC_PROCESSES));

    drop(interaction_guard);
    let wait_process_guard = exited_process.wait_process_lock().lock_owned().await;
    let pruned = {
        let mut store = manager.process_store.lock().await;
        let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);
        (pruned.map(|entry| entry.process_id), store.processes.len())
    };

    assert_eq!(pruned, (None, MAX_UNIFIED_EXEC_PROCESSES));

    drop(wait_process_guard);
    let operation_lease = exited_process.acquire_operation_lease();
    let pruned = {
        let mut store = manager.process_store.lock().await;
        let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);
        (pruned.map(|entry| entry.process_id), store.processes.len())
    };

    assert_eq!(pruned, (None, MAX_UNIFIED_EXEC_PROCESSES));
    drop(operation_lease);

    {
        let mut store = manager.process_store.lock().await;
        for (process_id, entry) in &mut store.processes {
            entry.process = Arc::clone(&live_process);
            entry.last_used = now
                + Duration::from_millis(
                    u64::try_from(*process_id).expect("positive process id should fit u64"),
                );
        }
        store
            .processes
            .get(&1)
            .expect("oldest process should be stored")
            .initial_exec_command_active
            .store(true, std::sync::atomic::Ordering::Release);
    }

    let pruned = {
        let mut store = manager.process_store.lock().await;
        let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);
        (
            pruned.map(|entry| entry.process_id),
            store.processes.contains_key(&1),
        )
    };

    assert_eq!(pruned, (Some(2), true));
}
