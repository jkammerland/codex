//! Remote forks must retain server roots after reloading the client's configuration.

use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_app_server;
use super::session_lifecycle_requests::start_recording_remote_app_server;
use super::*;
use crate::app_server_session::ThreadParamsMode;
use app_test_support::create_fake_rollout;
use codex_app_server_client::AppServerEvent;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn remote_fork_dispatch_preserves_server_workspace_roots() -> Result<()> {
    check_fork_dispatch(ThreadParamsMode::Remote).await
}

#[tokio::test]
async fn local_daemon_fork_dispatch_preserves_idle_source_and_children() -> Result<()> {
    check_fork_dispatch(ThreadParamsMode::Embedded).await
}

async fn check_fork_dispatch(mode: ThreadParamsMode) -> Result<()> {
    let mut app = Box::pin(make_test_app()).await;
    let client_home = tempdir()?;
    let client_workspace = tempdir()?;
    let server_home = tempdir()?;
    let workspace = tempdir()?;
    let remote_cwd = workspace.path().canonicalize()?.abs();
    let server_root = server_home.path().canonicalize()?.abs();
    for home in [&client_home, &server_home] {
        let root = serde_json::to_string(&home.path().canonicalize()?)?;
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "sandbox_mode = \"workspace-write\"\n[sandbox_workspace_write]\nwritable_roots = [{root}]\n"
            ),
        )?;
    }
    // Reselecting the remote source retains an explicit cwd override, so normal folder consent
    // applies. Trust this fixture workspace before testing fork state and pending-request replay.
    crate::legacy_core::config::set_project_trust_level(
        server_home.path(),
        remote_cwd.as_path(),
        codex_protocol::config_types::TrustLevel::Trusted,
    )
    .map_err(|error| color_eyre::eyre::eyre!("{error:#}"))?;

    // Load the written client policy before attachment so a later config reload is equivalent.
    let client_cwd = client_workspace.path().canonicalize()?.abs();
    let client_root = client_home.path().canonicalize()?.abs();
    let client_config = ConfigBuilder::default()
        .codex_home(client_home.path().to_path_buf())
        .loader_overrides(app.loader_overrides.clone())
        .harness_overrides(ConfigOverrides {
            cwd: Some(client_cwd.to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;
    let client_paths = (client_cwd.clone(), vec![client_cwd, client_root]);
    assert_eq!(
        (
            client_config.cwd.clone(),
            client_config.workspace_roots.clone()
        ),
        client_paths
    );
    app.config = client_config;
    app.local_settings = crate::local_settings::LocalSettings::from(&app.config);
    app.chat_widget = ChatWidget::new_with_app_event(ChatWidgetInit {
        requires_openai_auth: false,
        local_settings: app.local_settings.clone(),
        config: app.config.clone(),
        frame_requester: crate::tui::FrameRequester::test_dummy(),
        app_event_tx: app.app_event_tx.clone(),
        workspace_command_runner: None,
        initial_user_message: None,
        enhanced_keys_supported: false,
        has_chatgpt_account: false,
        has_codex_backend_auth: false,
        model_catalog: app.model_catalog.clone(),
        feedback: app.feedback.clone(),
        is_first_run: false,
        status_account_display: None,
        initial_plan_type: None,
        model: Some(app.chat_widget.current_model().to_string()),
        startup_tooltip_override: None,
        status_line_invalid_items_warned: app.status_line_invalid_items_warned.clone(),
        terminal_title_invalid_items_warned: app.terminal_title_invalid_items_warned.clone(),
        session_telemetry: app.session_telemetry.clone(),
    });
    let server_config = ConfigBuilder::default()
        .codex_home(server_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(remote_cwd.to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;
    let (server, requests, proxy) = match mode {
        ThreadParamsMode::Remote => {
            // Match a real remote attachment; its executor paths must not enter local config.
            app.app_server_target = AppServerTarget::Remote {
                endpoint: crate::RemoteAppServerEndpoint::WebSocket {
                    websocket_url: "ws://127.0.0.1:1".into(),
                    auth_token: None,
                },
            };
            app.chat_widget.remote_connection =
                crate::status::remote_connection::remote_connection_status_value(
                    &app.app_server_target,
                    /*server_version*/ None,
                );
            Box::pin(start_recording_remote_app_server(&server_config)).await?
        }
        ThreadParamsMode::Embedded => {
            app.config = server_config.clone();
            Box::pin(start_recording_app_server(
                &server_config,
                /*blocked_thread_list*/ None,
                /*failed_thread_name*/ None,
            ))
            .await?
        }
    };
    let mut server = server.with_remote_cwd_override(Some(remote_cwd.to_path_buf()));
    let source_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            server_home.path(),
            "2026-01-01T00-00-00",
            "2026-01-01T00:00:00Z",
            "Saved user message",
            Some(server_config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("create source rollout"),
    )?;
    let started = server
        .resume_thread(
            &app.local_settings,
            app.config.clone(),
            source_thread_id,
            app.resume_model_settings(),
        )
        .await?;
    let expected_roots = vec![remote_cwd, server_root];
    assert_eq!(
        started.session.runtime_workspace_roots,
        expected_roots
            .iter()
            .map(codex_utils_path_uri::LegacyAppPathString::from_abs_path)
            .collect::<Vec<_>>()
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.thread_event_listener_tasks
        .insert(source_thread_id, tokio::spawn(std::future::pending::<()>()));
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let pending = if mode == ThreadParamsMode::Remote {
        let approval = exec_approval_request(
            source_thread_id,
            "source-turn",
            "command",
            /*approval_id*/ None,
        );
        app.handle_app_server_event(
            &server,
            AppServerEvent::ServerRequest(Box::new(approval.clone())),
        )
        .await;
        app.drain_active_thread_events(&mut tui).await?;
        vec![approval]
    } else {
        Vec::new()
    };
    let child_id = ThreadId::new();
    app.upsert_agent_picker_thread(
        child_id, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::ForkCurrentSession { name: None },
    ))
    .await?;

    let forked_thread_id = app.chat_widget.thread_id().expect("attached fork");
    assert_ne!(forked_thread_id, source_thread_id);
    match mode {
        ThreadParamsMode::Remote => assert_eq!(
            (
                app.chat_widget.config_ref().cwd.clone(),
                app.chat_widget.config_ref().workspace_roots.clone(),
            ),
            client_paths
        ),
        ThreadParamsMode::Embedded => {
            assert_eq!(app.chat_widget.config_ref().workspace_roots, expected_roots);
        }
    }
    let fork_roots: Vec<_> = recorded_params(&requests, "thread/fork")
        .into_iter()
        .map(|params| params["runtimeWorkspaceRoots"].clone())
        .collect();
    assert_eq!(
        fork_roots,
        vec![match mode {
            ThreadParamsMode::Remote => serde_json::Value::Null,
            ThreadParamsMode::Embedded => serde_json::json!(expected_roots),
        }]
    );
    let forked_thread = server
        .thread_read(forked_thread_id, /*include_turns*/ false)
        .await?;
    assert_eq!(
        forked_thread.environments,
        Some(vec![codex_app_server_protocol::ThreadEnvironment {
            environment_id: "local".into(),
            cwd: expected_roots[0].clone().into(),
            runtime_workspace_roots: expected_roots
                .iter()
                .map(codex_utils_path_uri::LegacyAppPathString::from_abs_path)
                .collect(),
        }])
    );
    assert!(recorded_params(&requests, "thread/unsubscribe").is_empty());
    assert!(!app.thread_event_listener_tasks[&source_thread_id].is_finished());
    assert_eq!(
        app.agents_overview.dispatched_requests[&source_thread_id]
            .iter()
            .map(|pending| &pending.request)
            .collect::<Vec<_>>(),
        pending.iter().collect::<Vec<_>>()
    );
    assert!(app.agents_overview.dispatched_requests[&child_id].is_empty());
    let question = request_user_input_request(source_thread_id, "source-turn", "question");
    app.handle_app_server_event(
        &server,
        AppServerEvent::ServerRequest(Box::new(question.clone())),
    )
    .await;
    assert_eq!(
        app.agents_overview.dispatched_requests[&source_thread_id]
            .last()
            .map(|pending| &pending.request),
        Some(&question)
    );
    assert!(!app.chat_widget.has_active_view());
    Box::pin(app.select_agents_overview_thread(&mut tui, &mut server, source_thread_id)).await?;
    app.drain_active_thread_events(&mut tui).await?;
    assert!(
        app.pending_app_server_requests
            .contains_server_request(&question)
    );
    app.discard_thread_local_state(source_thread_id).await;
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
