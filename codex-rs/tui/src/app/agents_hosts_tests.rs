use super::*;
use crate::app::agents_overview::tests::overview_thread;
use crate::app::agents_overview_view::AgentsOverviewGrouping;
use crate::bottom_pane::BottomPaneView;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::ThreadArchivedNotification;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_app_server_protocol::ThreadStatus;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn key(machine: &str) -> HostKey {
    HostKey {
        machine_id: machine.into(),
        server_id: "main".into(),
    }
}

fn host(label: &str) -> AgentHost {
    AgentHost {
        label: label.into(),
        machine_label: label.split(" / ").next().unwrap().into(),
        target: AppServerTarget::Embedded,
        session: None,
        epoch: 1,
        connected: true,
        state: HostState::default(),
        buffered: VecDeque::new(),
        revision: 0,
        refresh_generation: 0,
        refreshing: false,
        refresh_metadata: Vec::new(),
    }
}

fn render(view: &impl Renderable) -> String {
    render_with_height(view, 24)
}

fn render_with_height(view: &impl Renderable, height: u16) -> String {
    let area = Rect::new(0, 0, 100, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    crate::chatwidget::tests::helpers::normalize_agent_center_snapshot(
        buffer
            .content
            .chunks(100)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[tokio::test]
async fn overview_groups_machines_and_routes_equal_thread_ids_to_the_selected_host() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    let alpha = key("alpha");
    let beta = key("beta");
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts.foreground = alpha.clone();
    app.agents_hosts.hosts.insert(alpha, host("Alpha / main"));
    let mut beta_host = host("Beta / main");
    let mut remote = overview_thread(id, None, "Remote task", ThreadStatus::SystemError);
    remote.updated_at = 4_000_000_000;
    beta_host
        .state
        .agents_overview
        .threads
        .insert(id, Some(remote));
    app.agents_hosts.hosts.insert(beta.clone(), beta_host);
    app.agents_overview.view_state.lock().unwrap().grouping = AgentsOverviewGrouping::Status;
    let mut local = overview_thread(id, None, "Local task", ThreadStatus::Idle);
    local.updated_at = 4_000_000_000;
    let mut view = app.agents_overview_view(vec![local], Some(id));
    insta::assert_snapshot!("agents_machines_status_groups", render(&view));
    view.handle_key_event(crossterm::event::KeyCode::Down.into());
    view.handle_key_event(crossterm::event::KeyCode::Enter.into());
    let AppEvent::AgentsHostAction { target, event } = events.try_recv().unwrap() else {
        panic!("expected a machine-qualified action")
    };
    assert_eq!(
        target,
        ThreadTarget {
            host: beta,
            thread_id: id
        }
    );
    assert!(
        matches!(*event, AppEvent::SelectAgentsOverviewThread { thread_id } if thread_id == id)
    );
}

#[tokio::test]
async fn delayed_listing_cannot_resurrect_archived_thread_or_overwrite_newer_metadata() -> Result<()>
{
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("remote");
    let live = ThreadId::from_u128(1);
    let archived = ThreadId::from_u128(2);
    let mut remote = host("Remote");
    for (id, title) in [(live, "Old title"), (archived, "Archived")] {
        remote.state.agents_overview.threads.insert(
            id,
            Some(overview_thread(id, None, title, ThreadStatus::Idle)),
        );
    }
    state::track_notification(
        &mut remote.state.agents_overview,
        &ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
            thread_id: live.to_string(),
            thread_name: Some("New title".into()),
        }),
    );
    state::track_notification(
        &mut remote.state.agents_overview,
        &ServerNotification::ThreadArchived(ThreadArchivedNotification {
            thread_id: archived.to_string(),
        }),
    );
    remote.revision = 2;
    app.agents_hosts.hosts.insert(machine.clone(), remote);
    app.handle_agents_host_update(
        &mut tui,
        &mut server,
        HostUpdate::Listed {
            scope: ConnectionScope {
                host: machine.clone(),
                epoch: 1,
            },
            revision: 0,
            generation: 0,
            result: Ok(vec![
                overview_thread(live, None, "Old title", ThreadStatus::Idle),
                overview_thread(archived, None, "Archived", ThreadStatus::Idle),
            ]),
        },
    )
    .await?;
    let retained: Vec<_> = app.agents_hosts.hosts[&machine]
        .state
        .agents_overview
        .threads
        .iter()
        .map(|(id, thread)| (*id, thread.as_ref().and_then(|thread| thread.name.clone())))
        .collect();
    assert_eq!(retained, vec![(live, Some("New title".into()))]);
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_host_epoch_and_foreground_completion_cannot_change_the_current_conversation()
-> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("remote");
    app.agents_hosts
        .hosts
        .insert(machine.clone(), host("Remote"));
    app.agents_hosts.generation = 3;
    app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::Scoped {
            scope: ConnectionScope {
                host: app.agents_hosts.foreground.clone(),
                epoch: 0,
            },
            generation: 2,
            event: Box::new(AppEvent::AccountEmailLoaded {
                request_id: Uuid::new_v4(),
                email: Some("stale@example.test".into()),
            }),
        },
    )
    .await?;
    app.handle_agents_host_update(
        &mut tui,
        &mut server,
        HostUpdate::Offline(ConnectionScope {
            host: machine.clone(),
            epoch: 0,
        }),
    )
    .await?;
    assert!(app.agents_hosts.hosts[&machine].connected);
    assert!(!app.reconnect.offline);
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn request_copies_keep_transport_alive_and_namespace_concurrent_rpc_ids() -> Result<()> {
    let (app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut original = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut first = original.request_copy();
    let mut second = original.request_copy();
    let ids = [
        original.next_request_id(),
        first.next_request_id(),
        second.next_request_id(),
    ];
    assert!(ids[0] != ids[1] && ids[0] != ids[2] && ids[1] != ids[2]);
    let expected = original.read_account().await?;
    drop(original);
    let account = first.read_account().await?;
    assert_eq!(account, expected);
    second.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_attachment_preserves_source_draft_configuration_and_navigation() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let source = key("source");
    let target = key("unavailable");
    let source_id = ThreadId::from_u128(7);
    app.agents_hosts.foreground = source.clone();
    app.agents_hosts
        .hosts
        .insert(source.clone(), host("Source"));
    app.agents_hosts
        .hosts
        .insert(target.clone(), host("Unavailable"));
    app.agents_hosts.prepare_generation = 1;
    app.primary_thread_id = Some(source_id);
    app.chat_widget.handle_paste("Source draft".into());
    let input = app.chat_widget.capture_thread_input_state();
    let model = app.config.model.clone();
    let cwd = app.config.cwd.clone();
    let request_copy = server.request_copy();
    app.handle_agents_host_update(
        &mut tui,
        &mut server,
        HostUpdate::Prepared {
            scope: ConnectionScope {
                host: target,
                epoch: 1,
            },
            generation: 1,
            result: Err("Remote task is unavailable".into()),
            session: request_copy,
        },
    )
    .await?;
    assert_eq!(app.agents_hosts.foreground, source);
    assert_eq!(app.primary_thread_id, Some(source_id));
    assert_eq!(
        (app.config.model.clone(), app.config.cwd.clone()),
        (model, cwd)
    );
    assert_eq!(app.chat_widget.capture_thread_input_state(), input);
    server.shutdown().await?;
    Ok(())
}

fn team_snapshot(id: ThreadId) -> TeamSnapshot {
    use codex_app_server_protocol::*;
    let address = TeamAddress {
        machine_id: "alpha".into(),
        server_id: "main".into(),
        thread_id: id.to_string(),
    };
    TeamSnapshot {
        board: Default::default(),
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 4,
            mode: TeamExecutionMode::Stopped,
        },
        servers: vec![codex_team::ServerStatus {
            machine_id: "alpha".into(),
            machine_label: "Alpha".into(),
            server_id: "main".into(),
            server_label: "Main".into(),
            installation_id: Some("alpha-installation".into()),
            connected: true,
            applied_revision: Some(4),
            error: None,
        }],
        agents: vec![codex_team::Agent {
            enrollment: TeamEnrollment {
                address: address.clone(),
                alias: "release leader".into(),
                manually_held: true,
                continuation_turn_id: Some("original-turn".into()),
            },
            status: "working".into(),
            current_model: Some("previous-model".into()),
            current_effort: Some("medium".into()),
            next_model: Some("host-applied-model".into()),
            next_effort: None,
            project: None,
            requested_selection: Some(TeamModelSelection {
                revision: 9,
                model: TeamSelection::Value {
                    value: "queued-model".into(),
                },
                effort: TeamSelection::Default,
            }),
            runtime: Some(TeamAgentRuntime {
                thread_id: id.to_string(),
                root_thread_id: id.to_string(),
                parent_thread_id: None,
                alias: "release leader".into(),
                loaded: true,
                archived: false,
                turn_id: Some("original-turn".into()),
                current_model: Some("previous-model".into()),
                current_effort: Some("medium".into()),
                next_model: Some("host-applied-model".into()),
                next_effort: None,
                model_error: None,
                model_origin: TeamSelectionOrigin::Explicit,
                effort_origin: TeamSelectionOrigin::Default,
                superseded_selection_revision: None,
                requested_selection_revision: Some(9),
                applied_selection_revision: Some(8),
                in_flight: 1,
                reconciling: true,
                manually_held: true,
                continuation_turn_id: Some("original-turn".into()),
                jobs: vec![TeamTrackedJob {
                    id: "owned-job".into(),
                    thread_id: id.to_string(),
                    server: "jobs".into(),
                    tool: "launch".into(),
                    handle: Some("owned-handle".into()),
                    status: TeamJobStatus::Unconfirmed,
                    detail: Some("Cancellation has not been confirmed.".into()),
                }],
            }),
        }],
        messages: vec![codex_team::MessageRecord {
            message: TeamMessage {
                id: "request-one".into(),
                sender: address.clone(),
                recipient: None,
                kind: TeamMessageKind::Board,
                body: "Review the compiler change and reply here.".into(),
                topic: Some("reviews".into()),
                reply_to: None,
                created_at: 1,
            },
            delivery: TeamDeliveryMode::NextTurn,
            targets: Some(vec![address]),
            published: true,
            error: None,
            external_url: None,
            receipts: BTreeMap::from([(
                "delivery-one".into(),
                TeamReceipt {
                    delivery_id: "delivery-one".into(),
                    thread_id: "reviewer".into(),
                    state: TeamReceiptState::NeedsReconciliation,
                    turn_id: Some("original-turn".into()),
                    detail: Some(
                        "Input is durable; inspect the original turn before continuing.".into(),
                    ),
                },
            )]),
        }],
        rejected_outbox: vec![codex_team::RejectedOutbox {
            machine_id: "beta".into(),
            server_id: "main".into(),
            entry: TeamOutboxEntry {
                sequence: 12,
                id: "invalid".into(),
                kind: "message".into(),
                payload: serde_json::json!({}),
            },
            reason: "recipient is not enrolled".into(),
        }],
    }
}

#[tokio::test]
async fn unified_overview_keeps_alias_model_revisions_manual_holds_and_unconfirmed_jobs_visible() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    let alpha = key("alpha");
    app.agents_hosts.foreground = alpha.clone();
    app.agents_hosts.hosts.insert(alpha, host("Alpha"));
    app.agents_hosts.manifest = Some("manifest.toml".into());
    app.agents_hosts.snapshot = Some(team_snapshot(id));
    let mut thread = overview_thread(id, None, "Saved task title", ThreadStatus::Idle);
    thread.updated_at = 4_000_000_000;
    let view = app.agents_overview_view(vec![thread], Some(id));
    // Give the complete family state room so the snapshot verifies queued policy
    // and cancellation details rather than only their overflow indicator.
    let output = render_with_height(&view, 40);
    let details = output
        .lines()
        .filter_map(|line| line.split_once('│').map(|(_, detail)| detail))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(output.contains("Queued: queued-model / model default"));
    assert!(details.contains("launch: Unconfirmed · Cancellation has not been confirmed."));
    insta::assert_snapshot!("agents_team_model_and_cancellation_state", output);
}

#[tokio::test]
async fn unified_board_preserves_delivery_reconciliation_and_rejected_outgoing_mail() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut snapshot = team_snapshot(ThreadId::from_u128(42));
    snapshot
        .board
        .entries
        .push(codex_app_server_protocol::TeamBoardEntry {
            message: snapshot.messages[0].message.clone(),
            conversation_id: "request-one".into(),
            append_seq: 1,
            author: codex_app_server_protocol::TeamBoardAuthor {
                identity: codex_app_server_protocol::TeamBoardIdentity::Agent {
                    address: snapshot.messages[0].message.sender.clone(),
                },
                name: "release leader".into(),
                machine_name: Some("Alpha".into()),
                legacy: false,
            },
            mentions: vec![],
            notifications: vec![],
        });
    app.agents_hosts.snapshot = Some(snapshot);
    app.open_agents_team_board();
    let board = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 100);
    app.open_agents_team_message("request-one".into());
    let detail = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 100);
    insta::assert_snapshot!(
        "agents_team_board_and_receipts",
        format!("{board}\n\nDelivery details:\n{detail}")
    );
}

#[tokio::test]
async fn board_team_control_snapshot_refresh_keeps_draft() -> Result<()> {
    use super::super::agents_overview_view::command_center::TeamControlSummary;
    use codex_app_server_protocol::TeamExecutionMode;

    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut snapshot = team_snapshot(ThreadId::from_u128(42));
    app.agents_hosts.snapshot = Some(snapshot.clone());
    app.open_agents_team_board();
    let draft = {
        let mut board = app.agents_hosts.board.lock().unwrap();
        board.compose(String::new());
        board
            .composer
            .as_mut()
            .unwrap()
            .handle_paste("Keep my draft".into());
        board.composer.as_ref().unwrap().snapshot_draft()
    };

    // Host acknowledgment and control updates do not need a changed Board revision.
    snapshot.control.mode = TeamExecutionMode::Paused;
    snapshot.control.revision += 1;
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::TeamSnapshot {
            generation: 1,
            snapshot,
        },
    )
    .await?;
    let board = app.agents_hosts.board.lock().unwrap();
    assert_eq!(
        (
            board.team_control,
            board.composer.as_ref().unwrap().snapshot_draft(),
        ),
        (
            Some(TeamControlSummary {
                mode: TeamExecutionMode::Paused,
                applied: 0,
                total: 1,
            }),
            draft,
        )
    );
    Ok(())
}

#[tokio::test]
async fn empty_discovery_session_has_no_server_or_account_transport() {
    let mut session = AppServerSession::discovery_placeholder();
    assert!(session.is_discovery_placeholder());
    assert!(!session.uses_embedded_app_server());
    assert!(session.try_request_handle().is_none());
    assert!(session.read_account().await.is_err());
    assert!(session.request_copy().try_request_handle().is_none());
    assert!(session.shutdown().await.is_ok());
}

#[tokio::test]
async fn offline_machines_render_in_the_existing_overview_and_keep_navigation_available()
-> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Discovered {
            generation: 0,
            hosts: vec![
                (key("alpha"), "Alpha / main".into(), "Alpha".into()),
                (key("beta"), "Beta / main".into(), "Beta".into()),
            ],
        },
    )
    .await?;
    let mut view = app.agents_overview_view(Vec::new(), None);
    insta::assert_snapshot!("agents_machines_all_offline", render(&view));
    view.handle_key_event(crossterm::event::KeyCode::Char('n').into());
    assert!(matches!(
        events.try_recv(),
        Ok(AppEvent::AgentsMachinesMenu)
    ));
    view.handle_key_event(crossterm::event::KeyCode::Esc.into());
    assert!(matches!(
        events.try_recv(),
        Ok(AppEvent::Exit(ExitMode::Immediate))
    ));
    assert!(session.try_request_handle().is_none());
    Ok(())
}

#[tokio::test]
async fn discovery_completions_follow_the_host_that_became_foreground() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let machine = key("alpha");
    let id = ThreadId::from_u128(41);
    app.agents_hosts
        .hosts
        .insert(machine.clone(), host("Alpha"));
    // The RPC was started while parked; its owner has since moved into App.
    app.agents_hosts.foreground = machine.clone();
    let scope = ConnectionScope {
        host: machine.clone(),
        epoch: 1,
    };
    let thread = overview_thread(id, None, "Discovered after attach", ThreadStatus::Idle);
    app.apply_agents_host_listing(scope.clone(), 0, 0, Ok(vec![thread.clone()]));
    app.apply_agents_host_pins(scope, 0, Some(vec![id]));
    assert_eq!(app.agents_overview.threads.get(&id), Some(&Some(thread)));
    assert_eq!(app.agents_overview.pinned_thread_ids, Some(vec![id]));
    assert!(
        app.agents_hosts.hosts[&machine]
            .state
            .agents_overview
            .threads
            .is_empty()
    );
}

#[tokio::test]
async fn route_started_before_manifest_mapping_keeps_its_original_host_identity() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("alpha");
    let id = ThreadId::from_u128(42);
    app.agents_hosts.initial_host = machine.clone();
    app.agents_hosts.foreground = machine.clone();
    app.agents_hosts
        .hosts
        .insert(machine.clone(), host("Alpha"));
    app.agents_overview.threads.insert(
        id,
        Some(overview_thread(id, None, "Old", ThreadStatus::Idle)),
    );
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Event {
            scope: ConnectionScope {
                host: HostKey::default(),
                epoch: 1,
            },
            event: AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
                    thread_id: id.to_string(),
                    thread_name: Some("New".into()),
                }),
            )),
        },
    )
    .await?;
    assert_eq!(
        app.agents_overview.threads[&id]
            .as_ref()
            .and_then(|thread| thread.name.as_deref()),
        Some("New")
    );
    assert_eq!(app.agents_hosts.hosts[&machine].revision, 1);
    Ok(())
}

#[tokio::test]
async fn reverted_notification_uses_normal_intake_and_does_not_edit_another_visible_task()
-> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("alpha");
    let reverted_id = ThreadId::from_u128(40);
    let visible_id = ThreadId::from_u128(41);
    app.agents_hosts.foreground = machine.clone();
    app.agents_hosts
        .hosts
        .insert(machine.clone(), host("Alpha"));
    app.active_thread_id = Some(visible_id);
    app.chat_widget.handle_paste("Other task draft".into());
    let draft = app.chat_widget.capture_thread_input_state();
    app.pending_host_prompt_reverts.insert(
        reverted_id,
        super::super::agents_hosts_revert::PendingHostPromptRevert {
            prompt: crate::chatwidget::UserMessage {
                text: "Restored prompt".into(),
                local_images: Vec::new(),
                remote_image_urls: Vec::new(),
                text_elements: Vec::new(),
                mention_bindings: Vec::new(),
            },
            retained_turns: Vec::new(),
            reverted: Some(codex_app_server_protocol::ThreadRevertResponse {
                thread: overview_thread(reverted_id, None, "Edited task", ThreadStatus::Idle),
                turns_backwards_cursor: None,
                items_backwards_cursor: None,
            }),
            request_id: Uuid::new_v4(),
            error: None,
            nth_user_message: 0,
            notified: false,
        },
    );
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Event {
            scope: ConnectionScope {
                host: machine,
                epoch: 1,
            },
            event: AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadReverted(
                    codex_app_server_protocol::ThreadRevertedNotification {
                        thread_id: reverted_id.to_string(),
                    },
                ),
            )),
        },
    )
    .await?;
    let completion = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::CompleteHostPromptRevert { .. }))
        .expect("sole intake queues revert completion");
    app.handle_event(&mut tui, &mut session, completion).await?;
    assert_eq!(app.active_thread_id, Some(visible_id));
    assert_eq!(app.chat_widget.capture_thread_input_state(), draft);
    assert!(app.pending_host_prompt_reverts[&reverted_id].notified);
    Ok(())
}

#[tokio::test]
async fn team_control_keys_work_inside_child_compose_views() -> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.open_agents_team_compose(super::super::agents_hosts_team::TeamCompose::Topic);
    for (key, expected) in [
        (6, codex_app_server_protocol::TeamExecutionMode::Paused),
        (7, codex_app_server_protocol::TeamExecutionMode::Running),
        (8, codex_app_server_protocol::TeamExecutionMode::Stopped),
    ] {
        app.handle_tui_event(
            &mut tui,
            &mut session,
            TuiEvent::Key(KeyCode::F(key).into()),
        )
        .await?;
        assert!(
            matches!(events.try_recv(), Ok(AppEvent::AgentsTeamRequest(codex_team::TeamRequest::Control { mode })) if mode == expected)
        );
    }
    assert!(app.chat_widget.has_active_view());
    Ok(())
}

#[tokio::test]
async fn prompt_revert_preparation_returns_to_the_loop_before_its_rpc_finishes() -> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let source = key("source");
    let other = key("other");
    let id = ThreadId::from_u128(90);
    app.agents_hosts.foreground = source.clone();
    app.agents_hosts.hosts.insert(source, host("Source"));
    app.agents_hosts.hosts.insert(other.clone(), host("Other"));
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts.started = true;
    app.active_thread_id = Some(id);
    app.ensure_thread_channel(id);
    let prompt = crate::chatwidget::UserMessage {
        text: "Original prompt".into(),
        local_images: Vec::new(),
        remote_image_urls: Vec::new(),
        text_elements: Vec::new(),
        mention_bindings: Vec::new(),
    };
    app.start_host_prompt_revert(&session, id, 0, prompt)
        .await?;
    // The current-thread task has not run its failing transport read yet.
    assert!(app.pending_host_prompt_reverts.contains_key(&id));
    app.handle_tui_event(&mut tui, &mut session, TuiEvent::Key(KeyCode::Esc.into()))
        .await?;
    assert!(
        app.chat_widget
            .selected_index_for_present_view(super::super::agents_overview::AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
    );
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Event {
            scope: ConnectionScope {
                host: other.clone(),
                epoch: 1,
            },
            event: AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
                    thread_id: id.to_string(),
                    thread_name: Some("Other host updated".into()),
                }),
            )),
        },
    )
    .await?;
    assert_eq!(app.agents_hosts.hosts[&other].revision, 1);
    let completion = events.recv().await.expect("host preparation completion");
    assert!(matches!(completion, AppEvent::AgentsHostUpdate(_)));
    app.handle_event(&mut tui, &mut session, completion).await?;
    assert!(app.pending_host_prompt_reverts.is_empty());
    Ok(())
}

#[tokio::test]
async fn parked_host_task_registration_is_acknowledged_without_switching_foreground() -> Result<()>
{
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let source = key("source");
    let id = ThreadId::from_u128(50);
    let mut source_host = host("Source");
    source_host.session = Some(server.request_copy());
    app.agents_hosts.hosts.insert(source.clone(), source_host);
    let foreground = app.agents_hosts.foreground.clone();
    let (registered, mut confirmation) = tokio::sync::oneshot::channel();
    app.recover_host_completion(
        ConnectionScope {
            host: source.clone(),
            epoch: 1,
        },
        AppEvent::DynamicToolThreadStarted {
            thread: overview_thread(id, None, "Background task", ThreadStatus::Idle),
            task_tools_available: true,
            registered,
        },
    );
    confirmation
        .try_recv()
        .expect("parked registration acknowledged immediately");
    assert!(server.task_tools_available(id));
    assert!(
        app.agents_hosts.hosts[&source]
            .state
            .agents_overview
            .threads
            .contains_key(&id)
    );
    assert_eq!(app.agents_hosts.foreground, foreground);
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cached_task_capabilities_are_qualified_by_machine_and_server() {
    let (app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(72);
    let mut alpha = AppServerSession::discovery_placeholder();
    let mut beta = AppServerSession::discovery_placeholder();
    alpha.scope_task_tool_cache(&app.config.codex_home, "alpha", "main");
    beta.scope_task_tool_cache(&app.config.codex_home, "beta", "main");
    alpha.remember_task_tool_thread(id);
    assert!(alpha.task_tools_available(id));
    assert!(!beta.task_tools_available(id));
    // Reopening the same host finds its persisted capability without sharing an in-memory set.
    let mut reopened = AppServerSession::discovery_placeholder();
    reopened.scope_task_tool_cache(&app.config.codex_home, "alpha", "main");
    assert!(reopened.task_tools_available(id));
}

#[tokio::test]
async fn parked_status_wakeups_stay_with_their_host_when_thread_ids_match() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("parked");
    let mut parked = host("Parked");
    let (updates, mut parked_receiver) = tokio::sync::broadcast::channel(4);
    parked.state.dynamic_tool_status_updates = Some(updates);
    app.agents_hosts.hosts.insert(machine.clone(), parked);
    let mut foreground_receiver = app.dynamic_tool_status_updates.subscribe();
    let status = codex_app_server_protocol::ThreadStatusChangedNotification {
        thread_id: ThreadId::from_u128(73).to_string(),
        status: ThreadStatus::Idle,
    };
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Event {
            scope: ConnectionScope {
                host: machine,
                epoch: 1,
            },
            event: AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadStatusChanged(status.clone()),
            )),
        },
    )
    .await?;
    assert_eq!(parked_receiver.try_recv()?, status);
    assert!(foreground_receiver.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn reconstructed_widget_callbacks_keep_their_origin_scope() -> Result<()> {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let scope = ConnectionScope {
        host: key("source"),
        epoch: 2,
    };
    app.app_event_tx = app.app_event_tx.clone().scoped(scope.clone(), 4);
    let config = app.config.clone();
    let init = app.chatwidget_init_for_forked_or_resumed_thread(
        &mut tui, config, /*initial_user_message*/ None,
    );
    let mut widget = ChatWidget::new_with_app_event(init);
    widget.show_selection_view(crate::bottom_pane::SelectionViewParams {
        items: vec![crate::bottom_pane::SelectionItem {
            name: "Confirm".into(),
            actions: vec![Box::new(|sender| sender.send(AppEvent::FollowTranscript))],
            ..Default::default()
        }],
        ..crate::bottom_pane::SelectionViewParams::picker()
    });
    widget.handle_key_event_with_activation_key(KeyCode::Enter.into(), None);
    let callback = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::Scoped { event, .. } if matches!(event.as_ref(), AppEvent::FollowTranscript)))
        .expect("widget callback retains its machine and attachment generation");
    let AppEvent::Scoped {
        scope: actual,
        generation,
        ..
    } = callback
    else {
        unreachable!()
    };
    assert_eq!((actual, generation), (scope, 4));
    Ok(())
}

#[tokio::test]
async fn machine_menu_can_create_the_first_task_without_a_thread_target() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let machine = key("beta");
    app.agents_hosts
        .hosts
        .insert(machine.clone(), host("Beta / main"));
    app.open_agents_machines_menu();
    insta::assert_snapshot!(
        "agents_machines_empty_host_creation",
        render(&app.chat_widget)
    );
    app.chat_widget
        .handle_key_event_with_activation_key(KeyCode::Down.into(), None);
    app.chat_widget
        .handle_key_event_with_activation_key(KeyCode::Enter.into(), None);
    let action = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::NewAgentsHostSession { .. }))
        .expect("a connected empty host offers new-task creation");
    let AppEvent::NewAgentsHostSession { host, cwd } = action else {
        unreachable!()
    };
    assert_eq!((host, cwd), (machine, None));
}

#[tokio::test]
async fn successful_configuration_clears_only_current_startup_notices() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = TeamClient::new(
        &format!("http://{}", listener.local_addr()?),
        "isolated-test".into(),
    )
    .expect("valid reserved coordinator URL");
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts
        .hosts
        .insert(key("alpha"), host("Alpha / main"));
    app.agents_hosts.snapshot = Some(team_snapshot(ThreadId::from_u128(42)));
    app.agents_hosts.configuration_generation = 2;
    app.agents_hosts.notice = Some("Connecting configured machines…".into());

    // An earlier discovery must not dismiss the current attempt's progress.
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Configured {
            generation: 1,
            client: client.clone(),
            hosts: Vec::new(),
            foreground: None,
        },
    )
    .await?;
    assert_eq!(
        (
            app.agents_hosts.client.is_none(),
            app.agents_hosts.notice.as_deref()
        ),
        (true, Some("Connecting configured machines…"))
    );

    // A successful current discovery clears progress, but not a later failure.
    for (notice, expected) in [
        ("Connecting configured machines…", None),
        ("Connecting to the team coordinator…", None),
        (
            "Machine thread listing failed",
            Some("Machine thread listing failed"),
        ),
    ] {
        app.agents_hosts.notice = Some(notice.into());
        app.handle_agents_host_update(
            &mut tui,
            &mut session,
            HostUpdate::Configured {
                generation: 2,
                client: client.clone(),
                hosts: Vec::new(),
                foreground: None,
            },
        )
        .await?;
        assert_eq!(
            (
                app.agents_hosts.client.is_some(),
                app.agents_hosts.notice.as_deref()
            ),
            (true, expected)
        );
        if notice == "Connecting configured machines…" {
            let view = app.agents_overview_view(Vec::new(), None);
            insta::assert_snapshot!("agents_machines_configuration_complete", render(&view));
        }
        app.agents_hosts.tasks.abort_all();
    }
    Ok(())
}

#[tokio::test]
async fn parked_host_discovery_includes_loaded_threads_without_user_previews() -> Result<()> {
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut foreground = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("board-host");
    let [listed_id, board_id, other_board_id, ephemeral_id, gone_id] =
        [101, 102, 103, 104, 105].map(ThreadId::from_u128);
    let listed = overview_thread(
        listed_id,
        /*parent_thread_id*/ None,
        "Prompt task",
        ThreadStatus::NotLoaded,
    );
    let mut board = overview_thread(
        board_id,
        /*parent_thread_id*/ None,
        "Board reviewer",
        ThreadStatus::Idle,
    );
    board.preview.clear();
    let mut other_board = overview_thread(
        other_board_id,
        /*parent_thread_id*/ None,
        "Board builder",
        ThreadStatus::Active {
            active_flags: Vec::new(),
        },
    );
    other_board.preview.clear();
    let mut ephemeral = overview_thread(
        ephemeral_id,
        /*parent_thread_id*/ None,
        "Temporary",
        ThreadStatus::Idle,
    );
    ephemeral.ephemeral = true;
    let expected = vec![listed.clone(), board.clone(), other_board.clone()];

    // The server deliberately omits loaded Board-only sessions from ordinary history.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server_listed = listed.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut round = 0;
        let mut loaded_cursors = Vec::new();
        let mut reads = Vec::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let params = request.params.unwrap_or_default();
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "host-discovery-test/1.0"}),
                "thread/list" if params["sectionId"] == codex_state::PINNED_THREAD_SECTION_ID => {
                    json!({"data": [], "nextCursor": null})
                }
                "thread/list" => {
                    round += 1;
                    json!({"data": [server_listed], "nextCursor": null})
                }
                "thread/loaded/list" => {
                    let cursor = params["cursor"].as_str().map(str::to_owned);
                    loaded_cursors.push(cursor.clone());
                    if round == 2 {
                        socket.send(Message::Text(json!({
                            "id": request.id,
                            "error": {"code": -32603, "message": "loaded listing unavailable"},
                        }).to_string().into())).await?;
                        continue;
                    }
                    match cursor.as_deref() {
                        None => json!({
                            "data": [listed_id, board_id, ephemeral_id],
                            "nextCursor": "loaded-page-2",
                        }),
                        Some("loaded-page-2") => json!({
                            "data": [board_id, gone_id, other_board_id],
                            "nextCursor": null,
                        }),
                        cursor => panic!("unexpected loaded cursor: {cursor:?}"),
                    }
                }
                "thread/read" => {
                    let params: codex_app_server_protocol::ThreadReadParams =
                        serde_json::from_value(params)?;
                    assert!(!params.include_turns);
                    let id = ThreadId::from_string(&params.thread_id)?;
                    reads.push(id);
                    if id == gone_id {
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": request.id,
                                    "error": {"code": -32602, "message": "thread is gone"},
                                })
                                .to_string()
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    let thread = if id == board_id {
                        &board
                    } else if id == other_board_id {
                        &other_board
                    } else if id == ephemeral_id {
                        &ephemeral
                    } else {
                        panic!("unexpected metadata read: {id}");
                    };
                    json!({"thread": thread})
                }
                method => panic!("discovery must not mutate or hydrate threads: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, color_eyre::Report>((loaded_cursors, reads))
    });

    let session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let mut parked = host("Board host / main");
    parked.session = Some(session);
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts.hosts.insert(machine.clone(), parked);

    // Successful discovery unions both sources; optional loaded-list failure retains history.
    for expected_listing in [&expected, &vec![listed]] {
        app.refresh_other_agents_hosts(&foreground);
        let update = loop {
            if let AppEvent::AgentsHostUpdate(update) = events.recv().await.expect("host listing")
                && matches!(update.as_ref(), HostUpdate::Listed { .. })
            {
                break *update;
            }
        };
        let HostUpdate::Listed { result, .. } = &update else {
            unreachable!()
        };
        assert_eq!(result.as_ref().unwrap(), expected_listing);
        app.handle_agents_host_update(&mut tui, &mut foreground, update)
            .await?;
        assert_eq!(
            app.agents_hosts.hosts[&machine]
                .state
                .agents_overview
                .threads,
            expected
                .iter()
                .map(|thread| (
                    ThreadId::from_string(&thread.id).unwrap(),
                    Some(thread.clone())
                ))
                .collect(),
        );
    }
    let view = app.agents_overview_view(Vec::new(), /*selected_thread_id*/ None);
    insta::assert_snapshot!("agents_machines_loaded_board_recipients", render(&view));

    while let Some(completed) = app.agents_hosts.tasks.join_next().await {
        completed?;
    }
    app.agents_hosts
        .hosts
        .get_mut(&machine)
        .unwrap()
        .session
        .take()
        .unwrap()
        .shutdown()
        .await?;
    assert_eq!(
        server.await??,
        (
            vec![None, Some("loaded-page-2".into()), None],
            vec![board_id, ephemeral_id, gone_id, other_board_id],
        )
    );
    Ok(())
}

#[tokio::test]
async fn unknown_loaded_status_coalesces_only_its_host_after_in_flight_listings() -> Result<()> {
    use crate::app_server_session::ThreadParamsMode;
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut foreground = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("resuming-host");
    let other_machine = key("other-host");
    let id = ThreadId::from_u128(106);
    let tombstone = ThreadId::from_u128(107);
    let known_id = ThreadId::from_u128(109);
    let temporary_id = ThreadId::from_u128(110);
    let known = overview_thread(
        known_id,
        /*parent_thread_id*/ None,
        "Known task",
        ThreadStatus::Idle,
    );
    let mut thread = overview_thread(
        id,
        /*parent_thread_id*/ None,
        "Resumed Board recipient",
        ThreadStatus::Idle,
    );
    thread.preview.clear();
    let expected = thread.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let (listing_started, first_listing) = tokio::sync::oneshot::channel();
    let (release_listing, listing_released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut listing_started = Some(listing_started);
        let mut listing_released = Some(listing_released);
        let mut loaded_lists = 0;
        let mut reads = Vec::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let params = request.params.unwrap_or_default();
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "host-resume-discovery-test/1.0"}),
                "thread/list" => json!({"data": [], "nextCursor": null}),
                "thread/loaded/list" => {
                    loaded_lists += 1;
                    if loaded_lists == 1 {
                        listing_started.take().unwrap().send(()).unwrap();
                        listing_released.take().unwrap().await?;
                        json!({"data": [], "nextCursor": null})
                    } else {
                        json!({"data": [id], "nextCursor": null})
                    }
                }
                "thread/read" => {
                    let params: codex_app_server_protocol::ThreadReadParams =
                        serde_json::from_value(params)?;
                    assert!(!params.include_turns);
                    reads.push(params.thread_id);
                    json!({"thread": thread})
                }
                method => panic!("discovery must not mutate or hydrate threads: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, color_eyre::Report>((loaded_lists, reads))
    });

    // Keep a second real host refresh in flight throughout the first host's follow-up.
    let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let other_endpoint =
        crate::resolve_remote_addr(&format!("ws://{}", other_listener.local_addr()?))?;
    let (other_listing_started, other_first_listing) = tokio::sync::oneshot::channel();
    let (release_other_listing, other_listing_released) = tokio::sync::oneshot::channel();
    let other_server = tokio::spawn(async move {
        let (stream, _) = other_listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut listing_started = Some(other_listing_started);
        let mut listing_released = Some(other_listing_released);
        let mut loaded_lists = 0;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "other-host-discovery-test/1.0"}),
                "thread/list" => json!({"data": [], "nextCursor": null}),
                "thread/loaded/list" => {
                    loaded_lists += 1;
                    if loaded_lists == 1 {
                        listing_started.take().unwrap().send(()).unwrap();
                        listing_released.take().unwrap().await?;
                    }
                    json!({"data": [], "nextCursor": null})
                }
                method => panic!("unexpected discovery request: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, color_eyre::Report>(loaded_lists)
    });
    let session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let mut parked = host("Resuming host / main");
    parked.session = Some(session);
    parked
        .state
        .agents_overview
        .removed_threads
        .insert(tombstone);
    parked
        .state
        .agents_overview
        .threads
        .insert(known_id, Some(known.clone()));
    let (temporary_notifications, _temporary_receiver) = tokio::sync::mpsc::unbounded_channel();
    parked
        .state
        .temporary_structured_requests
        .insert(temporary_id, temporary_notifications);
    app.agents_hosts.hosts.insert(machine.clone(), parked);
    let mut other_host = host("Other host / main");
    other_host.session = Some(AppServerSession::new(
        crate::connect_remote_app_server(other_endpoint).await?,
        ThreadParamsMode::Remote,
    ));
    app.agents_hosts
        .hosts
        .insert(other_machine.clone(), other_host);
    app.refresh_other_agents_hosts(&foreground);
    first_listing.await?;
    other_first_listing.await?;

    // Repeated resume notifications coalesce; unloaded and removed rows are not discovered.
    for (thread_id, status, pending) in [
        (tombstone, ThreadStatus::Idle, false),
        (ThreadId::from_u128(108), ThreadStatus::NotLoaded, false),
        (known_id, ThreadStatus::Idle, false),
        (temporary_id, ThreadStatus::Idle, false),
        (id, ThreadStatus::Idle, true),
        (id, ThreadStatus::Idle, true),
    ] {
        app.handle_agents_host_update(
            &mut tui,
            &mut foreground,
            HostUpdate::Event {
                scope: ConnectionScope {
                    host: machine.clone(),
                    epoch: 1,
                },
                event: AppServerEvent::ServerNotification(Box::new(
                    ServerNotification::ThreadStatusChanged(
                        codex_app_server_protocol::ThreadStatusChangedNotification {
                            thread_id: thread_id.to_string(),
                            status,
                        },
                    ),
                )),
            },
        )
        .await?;
        let parked = &app.agents_hosts.hosts[&machine];
        assert_eq!(
            (
                parked.refresh_generation,
                parked.refreshing,
                parked.state.agents_overview.refresh_pending
            ),
            (1, true, pending),
        );
        assert!(
            !app.agents_hosts.hosts[&other_machine]
                .state
                .agents_overview
                .refresh_pending
        );
    }
    release_listing.send(()).unwrap();

    for _ in 0..2 {
        let update = loop {
            if let AppEvent::AgentsHostUpdate(update) = events.recv().await.expect("host listing")
                && matches!(update.as_ref(), HostUpdate::Listed { .. })
            {
                break *update;
            }
        };
        app.handle_agents_host_update(&mut tui, &mut foreground, update)
            .await?;
        assert!(
            !app.agents_hosts.hosts[&other_machine]
                .state
                .agents_overview
                .refresh_pending
        );
    }
    assert_eq!(
        app.agents_hosts.hosts[&machine]
            .state
            .agents_overview
            .threads,
        HashMap::from([(id, Some(expected)), (known_id, Some(known))]),
    );
    release_other_listing.send(()).unwrap();
    loop {
        if let AppEvent::AgentsHostUpdate(update) = events.recv().await.expect("other host listing")
            && matches!(update.as_ref(), HostUpdate::Listed { .. })
        {
            app.handle_agents_host_update(&mut tui, &mut foreground, *update)
                .await?;
            break;
        }
    }
    while let Some(completed) = app.agents_hosts.tasks.join_next().await {
        completed?;
    }
    for key in [machine, other_machine] {
        app.agents_hosts
            .hosts
            .get_mut(&key)
            .unwrap()
            .session
            .take()
            .unwrap()
            .shutdown()
            .await?;
    }
    assert_eq!(server.await??, (2, vec![id.to_string()]));
    assert_eq!(other_server.await??, 1);
    Ok(())
}

#[tokio::test]
async fn failed_initial_configuration_can_retry_without_stale_results_replacing_it() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.agents_hosts.manifest = Some(
        app.config
            .codex_home
            .join("missing-manifest.toml")
            .to_path_buf(),
    );
    app.agents_hosts.configuration_generation = 1;
    app.agents_hosts.started = true;
    app.open_agents_overview(&session);
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::ConfigurationFailed {
            generation: 1,
            error: "Coordinator unavailable".into(),
        },
    )
    .await?;
    assert!(!app.agents_hosts.started);
    assert!(
        app.chat_widget
            .selected_index_for_present_view(super::super::agents_overview::AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
    );
    app.handle_event(
        &mut tui,
        &mut session,
        AppEvent::AgentsHostReconnect(HostKey::default()),
    )
    .await?;
    assert_eq!(
        (
            app.agents_hosts.started,
            app.agents_hosts.configuration_generation
        ),
        (true, 2)
    );
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::ConfigurationFailed {
            generation: 1,
            error: "Stale failure".into(),
        },
    )
    .await?;
    let stale = key("stale");
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Discovered {
            generation: 1,
            hosts: vec![(stale.clone(), "Stale".into(), "Stale".into())],
        },
    )
    .await?;
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Configured {
            generation: 1,
            client: TeamClient::new("http://127.0.0.1:1", "isolated-test".into())
                .expect("valid unused coordinator URL"),
            hosts: Vec::new(),
            foreground: Some(stale.clone()),
        },
    )
    .await?;
    assert!(app.agents_hosts.started);
    assert!(app.agents_hosts.client.is_none());
    assert!(!app.agents_hosts.hosts.contains_key(&stale));
    assert_eq!(app.agents_hosts.foreground, HostKey::default());
    assert_ne!(app.agents_hosts.notice.as_deref(), Some("Stale failure"));
    assert!(
        app.chat_widget
            .selected_index_for_present_view(super::super::agents_overview::AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn first_listing_replays_unknown_metadata_after_its_host_becomes_foreground() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("first-list");
    let id = ThreadId::from_u128(93);
    let archived = ThreadId::from_u128(94);
    let mut loading = host("First list");
    loading.refreshing = true;
    loading.refresh_generation = 7;
    app.agents_hosts.hosts.insert(machine.clone(), loading);
    let scope = ConnectionScope {
        host: machine.clone(),
        epoch: 1,
    };
    for notification in [
        ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
            thread_id: id.to_string(),
            thread_name: Some("Newest title".into()),
        }),
        ServerNotification::ThreadArchived(ThreadArchivedNotification {
            thread_id: archived.to_string(),
        }),
    ] {
        app.handle_agents_host_update(
            &mut tui,
            &mut session,
            HostUpdate::Event {
                scope: scope.clone(),
                event: AppServerEvent::ServerNotification(Box::new(notification)),
            },
        )
        .await?;
    }
    let mut owner = std::mem::take(&mut app.agents_hosts.hosts.get_mut(&machine).unwrap().state);
    owner.swap(&mut app);
    app.agents_hosts.foreground = machine.clone();
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        HostUpdate::Event {
            scope: scope.clone(),
            event: AppServerEvent::ServerNotification(Box::new(
                ServerNotification::ThreadStatusChanged(
                    codex_app_server_protocol::ThreadStatusChangedNotification {
                        thread_id: id.to_string(),
                        status: ThreadStatus::SystemError,
                    },
                ),
            )),
        },
    )
    .await?;
    let stale = overview_thread(id, None, "Old title", ThreadStatus::Idle);
    app.apply_agents_host_listing(
        scope,
        7,
        0,
        Ok(vec![
            stale.clone(),
            overview_thread(archived, None, "Archived", ThreadStatus::Idle),
        ]),
    );
    let mut expected = stale;
    expected.name = Some("Newest title".into());
    expected.status = ThreadStatus::SystemError;
    assert_eq!(app.agents_overview.threads.get(&id), Some(&Some(expected)));
    assert!(!app.agents_overview.threads.contains_key(&archived));
    assert!(app.agents_overview.removed_threads.contains(&archived));
    assert!(app.agents_hosts.hosts[&machine].refresh_metadata.is_empty());
    Ok(())
}

#[tokio::test]
async fn reconnect_releases_an_interrupted_listing_for_new_discovery() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    app.agents_hosts.client = Some(
        TeamClient::new("http://127.0.0.1:1", "isolated-test".into())
            .expect("valid unused coordinator URL"),
    );
    let machine = key("reconnecting");
    let mut interrupted = host("Reconnecting");
    interrupted.refreshing = true;
    interrupted.refresh_metadata.push((
        1,
        ServerNotification::ThreadArchived(ThreadArchivedNotification {
            thread_id: ThreadId::from_u128(96).to_string(),
        }),
    ));
    app.agents_hosts.hosts.insert(machine.clone(), interrupted);
    app.connect_agents_host(machine.clone());
    let host = &app.agents_hosts.hosts[&machine];
    assert_eq!(
        (
            host.epoch,
            host.connected,
            host.refreshing,
            host.refresh_metadata.is_empty()
        ),
        (2, false, false, true)
    );
}

#[tokio::test]
async fn queued_safe_completion_cannot_be_reauthorized_after_host_reconnect() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("origin");
    let mut origin = host("Origin");
    origin.session = Some(server.request_copy());
    app.agents_hosts.hosts.insert(machine.clone(), origin);
    let request_id = Uuid::new_v4();
    app.recover_host_completion(
        ConnectionScope {
            host: machine.clone(),
            epoch: 1,
        },
        AppEvent::AccountEmailLoaded {
            request_id,
            email: Some("old-epoch@example.test".into()),
        },
    );
    let host = app.agents_hosts.hosts.get_mut(&machine).unwrap();
    host.epoch = 2;
    let completion = host
        .state
        .pending_app_events
        .pop_front()
        .expect("origin completion retained");
    app.agents_hosts.foreground = machine;
    app.account_email_request_id = Some(request_id);
    app.handle_event(&mut tui, &mut server, completion).await?;
    assert_eq!(app.account_email_request_id, Some(request_id));
    server.shutdown().await?;
    Ok(())
}

#[path = "agents_hosts_execution_tests.rs"]
mod execution;
