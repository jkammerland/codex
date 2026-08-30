use crate::StartThreadOptions;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::thread_manager::ThreadManagerState;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn residency_slot_reservation_unloads_oldest_idle_v2_agent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("second resident slot should evict the first idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
}

#[tokio::test]
async fn residency_admission_cancels_while_waiting_for_delivery_gate() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let initial_cancellation = CancellationToken::new();
    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &initial_cancellation,
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_completed(first.thread.as_ref()).await;
    let delivery_permit = first
        .thread
        .session
        .agent_delivery_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold delivery gate");

    let cancellation = CancellationToken::new();
    let reservation = control.reserve_v2_residency_slot(
        &state,
        &config,
        /*protected_thread_id*/ None,
        &cancellation,
    );
    tokio::pin!(reservation);
    assert!(futures::poll!(&mut reservation).is_pending());
    cancellation.cancel();
    let Err(error) = reservation.await else {
        panic!("cancelled residency admission must stop waiting");
    };

    assert!(matches!(error.details(), CodexErrorDetails::Interrupted));
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    drop(delivery_permit);
    let retry_cancellation = CancellationToken::new();
    let retry = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &retry_cancellation,
        )
        .await
        .expect("cancellation must not leak residency capacity");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected retry to evict the original resident, got {err:?}"),
        },
        Ok(_) => panic!("cancelled scan lost the resident from capacity accounting"),
    }
    drop(retry);
}

#[tokio::test]
async fn interrupted_v2_agent_is_lost_after_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    mark_thread_interrupted(first.thread.as_ref()).await;

    let second_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("second resident slot should evict the first interrupted idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);
    mark_thread_completed(second.thread.as_ref()).await;

    let err = control
        .ensure_v2_agent_loaded(config, first.thread_id, /*parent*/ None)
        .await
        .expect_err("evicted interrupted agent should stay lost");
    match err.details() {
        CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
        _ => panic!("expected ThreadNotFound, got {err:?}"),
    }

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
    match manager.get_thread(first.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
}

#[tokio::test]
async fn shutdown_v2_agent_is_evictable_after_a_late_shutdown_completion() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let first_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    first
        .thread
        .session
        .send_event_raw(codex_protocol::protocol::Event {
            id: "late-shutdown".to_string(),
            msg: EventMsg::ShutdownComplete,
        })
        .await;
    clear_active_turn(first.thread.as_ref()).await;

    let replacement_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("late shutdown completion should be evictable");

    assert!(manager.get_thread(first.thread_id).await.is_err());
    drop(replacement_slot);
}

#[tokio::test]
async fn residency_does_not_evict_parent_before_nonterminal_child_reports_completion() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let parent_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("parent resident slot");
    let parent = state
        .spawn_new_thread_with_source(
            config.clone(),
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            /*history_mode*/ None,
            Some(root.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            /*reserved_thread_id*/ None,
        )
        .await
        .expect("spawn parent agent");
    parent_slot.commit(parent.thread_id);
    mark_thread_completed(parent.thread.as_ref()).await;

    let child_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("child resident slot");
    let child = state
        .spawn_new_thread_with_source(
            config.clone(),
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            /*history_mode*/ None,
            Some(parent.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            /*reserved_thread_id*/ None,
        )
        .await
        .expect("spawn child agent");
    child_slot.commit(child.thread_id);

    let Err(error) = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
    else {
        panic!("nonterminal child must protect its parent from eviction");
    };
    assert!(
        matches!(
            error.details(),
            CodexErrorDetails::AgentLimitReached { max_threads: 2 }
        ),
        "unexpected reservation error: {error:?}"
    );
    assert!(manager.get_thread(parent.thread_id).await.is_ok());
    assert!(manager.get_thread(child.thread_id).await.is_ok());

    mark_thread_completed(child.thread.as_ref()).await;
    let replacement_slot = control
        .reserve_v2_residency_slot(
            &state,
            &config,
            /*protected_thread_id*/ None,
            &CancellationToken::new(),
        )
        .await
        .expect("terminal child must make an idle resident evictable");
    match manager.get_thread(parent.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => {
                assert_eq!(*thread_id, parent.thread_id)
            }
            _ => panic!("expected idle parent to be evicted, got {err:?}"),
        },
        Ok(_) => panic!("terminal child did not unblock parent eviction"),
    }
    drop(replacement_slot);
}

async fn spawn_v2_subagent(
    control: &AgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::Other(label.to_string())),
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
            /*reserved_thread_id*/ None,
        )
        .await
        .expect("spawn v2 subagent")
}

async fn mark_thread_completed(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn mark_thread_interrupted(thread: &CodexThread) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn.sub_id.clone()),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            }),
        )
        .await;
    clear_active_turn(thread).await;
}

async fn clear_active_turn(thread: &CodexThread) {
    // The fixture has no task runner to clear the turn after the terminal event.
    *thread.session.active_turn.lock().await = None;
}
