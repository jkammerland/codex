//! Real session streams remain independent through polling, closure and replacement.

use super::*;
use crate::app::backend_sessions::BackendSessions;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use futures::FutureExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn backend_sessions_poll_three_sources_and_isolate_closed_or_paused_streams() -> Result<()> {
    let mut app = make_test_app().await;
    app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);
    let mut sessions = Vec::new();
    let mut proxies = Vec::new();
    let mut owners = Vec::new();
    let mut threads = Vec::new();
    for _ in 0..3 {
        let (mut session, _, proxy) =
            session_lifecycle_requests::start_recording_remote_app_server(&app.config).await?;
        let thread = session.start_thread(&app.config).await?.session.thread_id;
        assert!(
            session
                .thread_settings_update(ThreadSettingsUpdateParams {
                    thread_id: thread.to_string(),
                    effort: Some(ReasoningEffortConfig::High),
                    ..Default::default()
                })
                .await?
        );
        owners.push(session.backend_id());
        threads.push(thread);
        sessions.push(session);
        proxies.push(proxy);
    }
    let mut sessions = sessions.into_iter();
    let mut backends = BackendSessions::new(sessions.next().unwrap());
    for session in sessions {
        backends.insert(session);
    }
    let mut unpaused = HashSet::new();
    for _ in 0..2 {
        let (owner, event) =
            time::timeout(Duration::from_secs(5), backends.next_event(Some(owners[0]))).await?;
        assert_ne!(owner, owners[0]);
        assert_eq!(event.unwrap().backend_id, owner);
        unpaused.insert(owner);
    }
    assert_eq!(unpaused, HashSet::from([owners[1], owners[2]]));
    let (owner, event) = time::timeout(
        Duration::from_secs(5),
        backends.next_event(/*paused_backend*/ None),
    )
    .await?;
    assert_eq!(owner, owners[0]);
    assert_eq!(event.unwrap().backend_id, owner);
    // Cancel a pending poll, then close only the selected transport.
    assert!(
        backends
            .next_event(/*paused_backend*/ None)
            .now_or_never()
            .is_none()
    );
    proxies[0].abort();
    loop {
        let (owner, event) = time::timeout(
            Duration::from_secs(5),
            backends.next_event(/*paused_backend*/ None),
        )
        .await?;
        assert_eq!(owner, owners[0]);
        if event.is_none() {
            break;
        }
    }
    assert!(!backends.is_listening(owners[0]));
    for index in 1..3 {
        assert!(backends.is_listening(owners[index]));
        let _: ThreadSettingsUpdateResponse = backends
            .session(owners[index])
            .unwrap()
            .request_handle()
            .request_typed(ClientRequest::ThreadSettingsUpdate {
                request_id: RequestId::Integer(900),
                params: ThreadSettingsUpdateParams {
                    thread_id: threads[index].to_string(),
                    effort: Some(ReasoningEffortConfig::Low),
                    ..Default::default()
                },
            })
            .await?;
    }
    let mut surviving = HashSet::new();
    for _ in 0..2 {
        let (owner, event) = time::timeout(
            Duration::from_secs(5),
            backends.next_event(/*paused_backend*/ None),
        )
        .await?;
        assert_eq!(event.unwrap().backend_id, owner);
        surviving.insert(owner);
    }
    assert_eq!(surviving, HashSet::from([owners[1], owners[2]]));
    // EOF is reported once, and a later cancelled poll does not spin on the closed source.
    assert!(
        backends
            .next_event(/*paused_backend*/ None)
            .now_or_never()
            .is_none()
    );

    let replacement = crate::start_app_server_for_picker(
        owners[0],
        &app.config,
        &AppServerTarget::Embedded,
        Vec::new(),
        LoaderOverrides::without_managed_config_for_tests(),
        /*state_db*/ None,
        Arc::clone(&app.environment_manager),
    )
    .await?;
    let reconnected_slot = backends.slot(owners[0]).unwrap();
    let previous = std::mem::replace(backends.session_at_mut(reconnected_slot), replacement);
    let _ = previous.shutdown().await;
    backends.resume_events(owners[0]);
    let thread = backends
        .session_at_mut(reconnected_slot)
        .start_thread(&app.config)
        .await?
        .session
        .thread_id;
    loop {
        let (owner, event) = time::timeout(
            Duration::from_secs(5),
            backends.next_event(/*paused_backend*/ None),
        )
        .await?;
        assert_eq!(owner, owners[0]);
        if let codex_app_server_client::AppServerEvent::ServerNotification(notification) =
            event.unwrap().event
            && let ServerNotification::ThreadStarted(started) = *notification
        {
            assert_eq!(started.thread.id, thread.to_string());
            break;
        }
    }
    backends.shutdown().await?;
    for (index, proxy) in proxies.into_iter().enumerate() {
        if index == 0 {
            assert!(proxy.await.unwrap_err().is_cancelled());
        } else {
            proxy.await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn foreground_slot_accepts_new_backend_events_after_previous_backend_eof() -> Result<()> {
    let mut app = make_test_app().await;
    let (session, _, proxy) =
        session_lifecycle_requests::start_recording_remote_app_server(&app.config).await?;
    let original_owner = session.backend_id();
    let mut backends = BackendSessions::new(session);
    let foreground_slot = backends
        .slot(original_owner)
        .expect("initial foreground slot");
    proxy.abort();
    loop {
        let (owner, event) = time::timeout(
            Duration::from_secs(5),
            backends.next_event(/*paused_backend*/ None),
        )
        .await?;
        assert_eq!(owner, original_owner);
        if event.is_none() {
            break;
        }
    }
    assert!(!backends.is_listening(original_owner));

    let mut replacement = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let displayed_owner = replacement.backend_id();
    let started = replacement.start_thread(&app.config).await?;
    let previous = std::mem::replace(backends.session_at_mut(foreground_slot), replacement);
    previous.shutdown().await?;
    assert!(backends.slot(original_owner).is_none());
    backends.resume_events(displayed_owner);

    loop {
        let (owner, event) = time::timeout(
            Duration::from_secs(5),
            backends.next_event(/*paused_backend*/ None),
        )
        .await?;
        assert_eq!(owner, displayed_owner);
        let event = event.expect("replacement stream remains open");
        let is_started = matches!(
            &event.event,
            codex_app_server_client::AppServerEvent::ServerNotification(notification)
                if matches!(notification.as_ref(), ServerNotification::ThreadStarted(_))
        );
        app.handle_polled_backend_event(
            backends.session_at(foreground_slot),
            displayed_owner,
            event,
        )
        .await;
        if is_started {
            break;
        }
    }
    assert!(
        app.agents_overview
            .threads
            .get(&started.session.thread_id)
            .is_some_and(Option::is_some)
    );
    backends.shutdown().await?;
    assert!(proxy.await.unwrap_err().is_cancelled());
    Ok(())
}

#[tokio::test]
async fn manifest_startup_drain_follows_direct_intake_before_and_after_host_registration()
-> Result<()> {
    for registered in [false, true] {
        let mut app = make_test_app().await;
        app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
        app.pending_startup_thread_start = true;
        app.app_server_target = AppServerTarget::Remote {
            endpoint: crate::resolve_remote_addr("ws://127.0.0.1:1")?,
        };
        if registered {
            app.agents_hosts.hosts.insert(
                app.agents_hosts.foreground.clone(),
                crate::app::agents_hosts::AgentHost {
                    label: "Direct foreground".into(),
                    machine_label: "Direct foreground".into(),
                    target: app.app_server_target.clone(),
                    session: None,
                    epoch: 1,
                    connected: true,
                    state: Default::default(),
                    buffered: VecDeque::new(),
                    revision: 0,
                    refresh_generation: 0,
                    refreshing: false,
                    refresh_metadata: Vec::new(),
                },
            );
        }
        let (session, requests, proxy) =
            session_lifecycle_requests::start_recording_remote_app_server(&app.config).await?;
        let owner = session.backend_id();
        let mut backends = BackendSessions::new(session);
        let slot = backends.slot(owner).expect("foreground slot");
        assert!(app.begin_reconnect_for_rejected_startup());

        // Discovery configuration cannot authorize a retry while the actual intake is open.
        assert!(app.startup_connection_is_draining(&backends, slot));
        assert!(session_lifecycle_requests::recorded_params(&requests, "thread/start").is_empty());
        proxy.abort();
        loop {
            let (event_owner, event) = time::timeout(
                Duration::from_secs(5),
                backends.next_event(/*paused_backend*/ None),
            )
            .await?;
            assert_eq!(event_owner, owner);
            if event.is_none() {
                break;
            }
        }
        // A registered host's connected flag can still be stale at direct EOF.
        assert!(!app.startup_connection_is_draining(&backends, slot));
        assert!(app.pending_startup_thread_start);
        backends.shutdown().await?;
        assert!(proxy.await.unwrap_err().is_cancelled());
    }
    Ok(())
}
