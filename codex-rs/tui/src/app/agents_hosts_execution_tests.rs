use super::*;
use crate::app::agents_hosts_execution::team_execution_held_error;
use crate::chatwidget::team_execution::TeamExecution;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::TeamExecutionMode;
use pretty_assertions::assert_eq;

#[test]
fn team_held_rejection_uses_structured_reason_or_exact_method_specific_legacy_contract() {
    for (method, legacy) in [
        (
            "turn/start",
            "failed to submit turn input: session execution is held by team control",
        ),
        (
            "turn/steer",
            "failed to steer turn: session execution is held by team control",
        ),
    ] {
        for (code, message, data, expected) in [
            (
                -32600,
                "Human wording may change",
                Some(serde_json::json!({"reason":"teamExecutionHeld"})),
                true,
            ),
            (-32603, legacy, None, true),
            (-32603, legacy, Some(serde_json::json!({})), false),
            (-32600, legacy, None, false),
            (
                -32603,
                "unrelated session execution is held by team control",
                None,
                false,
            ),
            (
                -32603,
                "session execution is held by team control",
                None,
                false,
            ),
            (
                -32600,
                legacy,
                Some(serde_json::json!({"reason":"other"})),
                false,
            ),
        ] {
            let error = TypedRequestError::Server {
                method: method.into(),
                source: JSONRPCErrorError {
                    code,
                    message: message.into(),
                    data,
                },
            };
            assert_eq!(team_execution_held_error(&error), expected, "{error:?}");
            let report = color_eyre::eyre::Report::new(error).wrap_err("turn/start failed in TUI");
            assert_eq!(
                report
                    .downcast_ref::<TypedRequestError>()
                    .is_some_and(team_execution_held_error),
                expected
            );
        }
    }
    let wrong_method = TypedRequestError::Server {
        method: "thread/read".into(),
        source: JSONRPCErrorError {
            code: -32600,
            message: "held".into(),
            data: Some(serde_json::json!({"reason":"teamExecutionHeld"})),
        },
    };
    assert!(!team_execution_held_error(&wrong_method));
}

#[tokio::test]
async fn team_projection_is_host_and_thread_qualified_and_manual_hold_is_not_team_hold() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    app.agents_hosts.foreground = key("alpha");
    app.agents_hosts.snapshot = Some(team_snapshot(id));
    assert_eq!(
        app.team_execution_for_thread(id),
        Some(TeamExecution::Stopped)
    );
    assert_eq!(app.team_execution_for_thread(ThreadId::from_u128(43)), None);
    app.agents_hosts.foreground = key("beta");
    assert_eq!(app.team_execution_for_thread(id), None);
    app.agents_hosts.foreground = HostKey {
        machine_id: "alpha".into(),
        server_id: "other".into(),
    };
    assert_eq!(app.team_execution_for_thread(id), None);
    app.agents_hosts.foreground = key("alpha");
    app.agents_hosts.snapshot.as_mut().unwrap().control.mode = TeamExecutionMode::Running;
    assert_eq!(
        app.team_execution_for_thread(id),
        Some(TeamExecution::Resuming)
    );
    app.agents_hosts.snapshot.as_mut().unwrap().agents[0]
        .runtime
        .as_mut()
        .unwrap()
        .reconciling = false;
    assert_eq!(
        app.team_execution_for_thread(id),
        Some(TeamExecution::Running)
    );
    app.agents_hosts.snapshot.as_mut().unwrap().servers[0].applied_revision = Some(3);
    assert_eq!(
        app.team_execution_for_thread(id),
        Some(TeamExecution::Syncing)
    );
}

#[tokio::test]
async fn team_snapshot_rejects_stale_read_and_f7_preserves_draft_until_running() -> Result<()> {
    let (mut app, mut events, mut operations) =
        crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    app.active_thread_id = Some(id);
    app.agents_hosts.foreground = key("alpha");
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            id,
            app.config.cwd.to_path_buf(),
        ));
    app.chat_widget.apply_external_edit("retained draft".into());
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let stopped = team_snapshot(id);
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 2,
            snapshot: stopped.clone(),
        },
    )
    .await?;
    assert!(app.chat_widget.team_execution_held());
    let mut running = stopped;
    running.control.mode = TeamExecutionMode::Running;
    running.agents[0].runtime.as_mut().unwrap().reconciling = false;
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 1,
            snapshot: running.clone(),
        },
    )
    .await?;
    assert!(app.chat_widget.team_execution_held());
    while events.try_recv().is_ok() {}
    app.handle_tui_event(&mut tui, &mut session, TuiEvent::Key(KeyCode::F(7).into()))
        .await?;
    assert!(matches!(
        events.try_recv(),
        Ok(AppEvent::AgentsTeamRequest(
            codex_team::TeamRequest::Control {
                mode: TeamExecutionMode::Running
            }
        ))
    ));
    assert!(app.chat_widget.team_execution_held());
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 3,
            snapshot: running,
        },
    )
    .await?;
    assert!(!app.chat_widget.team_execution_held());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    while let Ok(op) = operations.try_recv() {
        assert!(!matches!(op, AppCommand::UserTurn { .. }));
    }
    Ok(())
}

#[tokio::test]
async fn team_stop_between_composer_and_dispatch_restores_without_rpc() -> Result<()> {
    let (mut app, _, mut operations) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    app.active_thread_id = Some(id);
    app.agents_hosts.foreground = key("alpha");
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            id,
            app.config.cwd.to_path_buf(),
        ));
    let op = app
        .chat_widget
        .submit_user_message_as_plain_user_turn("race draft".into())
        .unwrap();
    while operations.try_recv().is_ok() {}
    app.agents_hosts.snapshot = Some(team_snapshot(id));
    app.sync_chat_team_execution();
    // A discovery placeholder has no request transport; reaching an RPC would fail.
    let mut session = AppServerSession::discovery_placeholder();
    assert!(
        crate::app::user_turn_submission::try_submit_user_turn(&mut app, &mut session, id, &op)
            .await?
    );
    assert_eq!(app.chat_widget.composer_text_with_pending(), "race draft");
    assert!(!app.chat_widget.is_user_turn_pending_or_running());
    assert!(operations.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn team_held_rejection_after_selection_restores_only_its_retained_task() {
    let (mut app, _events, mut operations) = crate::app::tests::make_test_app_with_channels().await;
    let rejected_id = ThreadId::from_u128(42);
    let selected_id = ThreadId::from_u128(43);
    app.active_thread_id = Some(rejected_id);
    app.agents_hosts.foreground = key("alpha");
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            rejected_id,
            app.config.cwd.to_path_buf(),
        ));
    // Session setup may request metadata before the prompt is submitted.
    while let Ok(op) = operations.try_recv() {
        assert!(!matches!(op, AppCommand::UserTurn { .. }));
    }
    let op = app
        .chat_widget
        .submit_user_message_as_plain_user_turn("original task draft".into())
        .unwrap();
    assert_eq!(operations.try_recv().unwrap(), op);
    let AppCommand::UserTurn {
        client_user_message_id,
        ..
    } = op
    else {
        panic!("user turn")
    };
    app.agents_overview.input_states.insert(
        rejected_id,
        app.chat_widget.capture_thread_input_state().unwrap(),
    );
    let (mut selected, _, _, _) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    selected.handle_thread_session(crate::app::tests::test_thread_session(
        selected_id,
        app.config.cwd.to_path_buf(),
    ));
    selected.apply_external_edit("selected task draft".into());
    app.active_thread_id = Some(selected_id);
    app.agents_hosts.snapshot = Some(team_snapshot(rejected_id));
    app.replace_chat_widget(selected);
    assert!(!app.chat_widget.team_execution_held());
    app.restore_team_held_submission(rejected_id, &client_user_message_id)
        .await;
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "selected task draft"
    );
    let recovered = app
        .agents_overview
        .input_states
        .remove(&rejected_id)
        .unwrap();
    let (mut original, _, _, _) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    original.handle_thread_session(crate::app::tests::test_thread_session(
        rejected_id,
        app.config.cwd.to_path_buf(),
    ));
    original.restore_thread_input_state(
        Some(recovered),
        crate::chatwidget::ThreadInputStateRestoreMode {
            preserve_in_flight_turn: true,
        },
    );
    app.active_thread_id = Some(rejected_id);
    app.replace_chat_widget(original);
    assert!(app.chat_widget.team_execution_held());
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "original task draft"
    );
}

#[tokio::test]
async fn team_held_rejection_survives_cached_navigation_and_voice_owner_return() {
    for voice_active in [false, true] {
        let (mut app, _events, mut operations) =
            crate::app::tests::make_test_app_with_channels().await;
        let rejected_id = ThreadId::from_u128(42);
        let selected_id = ThreadId::from_u128(43);
        app.active_thread_id = Some(rejected_id);
        app.agents_hosts.foreground = key("alpha");
        app.chat_widget
            .handle_thread_session(crate::app::tests::test_thread_session(
                rejected_id,
                app.config.cwd.to_path_buf(),
            ));
        // Session setup may request metadata before the prompt is submitted.
        while let Ok(op) = operations.try_recv() {
            assert!(!matches!(op, AppCommand::UserTurn { .. }));
        }
        let op = app
            .chat_widget
            .submit_user_message_as_plain_user_turn("cached draft".into())
            .unwrap();
        assert_eq!(operations.try_recv().unwrap(), op);
        let AppCommand::UserTurn {
            client_user_message_id,
            ..
        } = op
        else {
            panic!("user turn")
        };
        app.ensure_thread_channel(rejected_id);
        app.store_active_thread_receiver().await;

        let (mut selected, _, _, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        selected.handle_thread_session(crate::app::tests::test_thread_session(
            selected_id,
            app.config.cwd.to_path_buf(),
        ));
        selected.apply_external_edit("selected draft".into());
        app.active_thread_id = Some(selected_id);
        let mut original = std::mem::replace(&mut app.chat_widget, selected);
        if voice_active {
            original.park_voice();
            app.background_voice = Some(Box::new(original));
        }
        app.restore_team_held_submission(rejected_id, &client_user_message_id)
            .await;
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            "selected draft"
        );

        // Return uses the same canonical snapshot as ordinary cached thread navigation.
        let snapshot = app.thread_event_channels[&rejected_id]
            .store
            .lock()
            .await
            .snapshot();
        app.active_thread_id = Some(rejected_id);
        let (mut restored, _, _, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        restored.handle_thread_session(crate::app::tests::test_thread_session(
            rejected_id,
            app.config.cwd.to_path_buf(),
        ));
        app.replace_chat_widget(restored);
        app.replay_thread_snapshot(snapshot, /*resume_restored_queue*/ false);
        assert!(app.background_voice.is_none());
        assert_eq!(app.chat_widget.composer_text_with_pending(), "cached draft");
        assert!(!app.chat_widget.is_user_turn_pending_or_running());
    }
}

#[tokio::test]
async fn team_rejection_survives_navigation_until_newer_running_snapshot() -> Result<()> {
    let (mut app, _, mut operations) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    let other = ThreadId::from_u128(43);
    app.active_thread_id = Some(id);
    app.agents_hosts.foreground = key("alpha");
    app.chat_widget
        .handle_thread_session(crate::app::tests::test_thread_session(
            id,
            app.config.cwd.to_path_buf(),
        ));
    let op = app
        .chat_widget
        .submit_user_message_as_plain_user_turn("rejected draft".into())
        .unwrap();
    let AppCommand::UserTurn {
        client_user_message_id,
        ..
    } = op
    else {
        panic!("user turn")
    };
    app.chat_widget.queue_user_message_with_options(
        "old queue".into(),
        crate::bottom_pane::QueuedInputAction::Plain,
        Vec::new(),
    );
    while operations.try_recv().is_ok() {}
    let mut running = team_snapshot(id);
    running.control.mode = TeamExecutionMode::Running;
    running.agents[0].runtime.as_mut().unwrap().reconciling = false;
    app.agents_hosts.snapshot = Some(running.clone());
    app.agents_hosts.applied_team_read = 5;
    app.agents_hosts
        .team_read_generation
        .store(6, std::sync::atomic::Ordering::Relaxed);
    app.restore_team_held_submission(id, &client_user_message_id)
        .await;
    app.ensure_thread_channel(id);
    app.store_active_thread_receiver().await;

    let (mut selected, _, _, _) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    selected.handle_thread_session(crate::app::tests::test_thread_session(
        other,
        app.config.cwd.to_path_buf(),
    ));
    app.active_thread_id = Some(other);
    app.replace_chat_widget(selected);
    assert!(!app.chat_widget.team_execution_held());
    let snapshot = app.thread_event_channels[&id].store.lock().await.snapshot();
    let (mut restored, _, _, mut restored_operations) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    restored.handle_thread_session(crate::app::tests::test_thread_session(
        id,
        app.config.cwd.to_path_buf(),
    ));
    app.active_thread_id = Some(id);
    app.replace_chat_widget(restored);
    app.replay_thread_snapshot(snapshot, /*resume_restored_queue*/ true);
    assert!(app.chat_widget.team_execution_held());
    while let Ok(op) = restored_operations.try_recv() {
        assert!(!matches!(op, AppCommand::UserTurn { .. }));
    }
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "rejected draft"
    );

    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 6,
            snapshot: running,
        },
    )
    .await?;
    assert!(app.chat_widget.team_execution_held());
    while let Ok(op) = restored_operations.try_recv() {
        assert!(!matches!(op, AppCommand::UserTurn { .. }));
    }
    let latest = app.agents_hosts.snapshot.clone().unwrap();
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 7,
            snapshot: latest,
        },
    )
    .await?;
    let op = std::iter::from_fn(|| restored_operations.try_recv().ok())
        .find(|op| matches!(op, AppCommand::UserTurn { .. }))
        .expect("old queue resumes");
    let AppCommand::UserTurn { items, .. } = op else {
        unreachable!()
    };
    assert_eq!(
        items,
        vec![codex_app_server_protocol::UserInput::Text {
            text: "old queue".into(),
            text_elements: Vec::new()
        }]
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "rejected draft"
    );
    Ok(())
}

#[tokio::test]
async fn team_held_rejection_before_discovery_survives_widget_replacement() {
    for missing_snapshot in [true, false] {
        let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
        let id = ThreadId::from_u128(42);
        app.active_thread_id = Some(id);
        app.agents_hosts.foreground = key("alpha");
        if !missing_snapshot {
            let mut snapshot = team_snapshot(id);
            snapshot.control.mode = TeamExecutionMode::Running;
            snapshot.agents.clear();
            app.agents_hosts.snapshot = Some(snapshot);
        }
        app.restore_team_held_submission(id, "request-before-discovery")
            .await;
        let (mut replacement, _, _, _) =
            crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
        replacement.handle_thread_session(crate::app::tests::test_thread_session(
            id,
            app.config.cwd.to_path_buf(),
        ));
        app.replace_chat_widget(replacement);
        assert!(app.chat_widget.team_execution_held());
        app.agents_hosts.foreground = key("beta");
        app.sync_chat_team_execution();
        assert!(!app.chat_widget.team_execution_held());
    }
}

#[tokio::test]
async fn team_held_rejection_follows_discovery_foreground_identity() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    app.active_thread_id = Some(id);
    app.agents_hosts
        .hosts
        .insert(HostKey::default(), host("Local"));
    app.agents_hosts.applied_team_read = 3;
    app.restore_team_held_submission(id, "before-discovery")
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = TeamClient::new(
        &format!("http://{}", listener.local_addr()?),
        "isolated-test".into(),
    )
    .unwrap();
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Configured {
            generation: 0,
            client,
            hosts: Vec::new(),
            foreground: Some(key("alpha")),
        },
    )
    .await?;
    assert_eq!(app.agents_hosts.foreground, key("alpha"));
    assert_eq!(
        app.agents_hosts.held_rejections,
        HashMap::from([(
            ThreadTarget {
                host: key("alpha"),
                thread_id: id
            },
            3
        )])
    );
    assert_eq!(
        app.team_execution_for_thread(id),
        Some(TeamExecution::Syncing)
    );
    Ok(())
}
