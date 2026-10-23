use super::*;
use codex_app_server_protocol::TeamBoardPurgeOperation;
use codex_app_server_protocol::TeamBoardPurgeScope;
use codex_app_server_protocol::TeamBoardPurgeWorker;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn retirement_progress_shows_why_removal_is_waiting() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut operation = codex_app_server_protocol::TeamAdministrationOperation {
        operation_id: "remove-42".into(),
        actor: codex_app_server_protocol::TeamAddress {
            machine_id: "laptop".into(),
            server_id: "primary".into(),
            thread_id: "reviewer".into(),
        },
        request: TeamAdministrationRequest::Member(TeamMemberActionParams::Remove {
            address: TeamAgentTarget::Alias("Builder".into()),
        }),
        state: TeamAdministrationState::Waiting,
        waiting_reason: Some("Waiting for the current turn to finish.".into()),
        result: None,
    };
    app.agents_hosts.snapshot = Some(snapshot(vec![TeamOperatorOperation::Administration(
        operation.clone(),
    )]));
    app.open_agents_operator_operation(operation.operation_id.clone());
    let pending = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 70);

    operation.state = TeamAdministrationState::Succeeded;
    operation.waiting_reason = None;
    app.agents_hosts.snapshot = Some(snapshot(vec![TeamOperatorOperation::Administration(
        operation,
    )]));
    app.refresh_agents_operator_operation(None);
    let complete = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 70);
    insta::assert_snapshot!(
        "member_removal_operation_progress",
        format!("Waiting\n{pending}\n\nComplete\n{complete}")
    );
}

fn purge() -> TeamOperatorOperation {
    TeamOperatorOperation::BoardPurge(TeamBoardPurgeOperation {
        operation_id: "purge-42".into(),
        scope: TeamBoardPurgeScope::All,
        revision: 42,
        conversation_count: 3,
        message_count: 17,
        workers: vec![TeamBoardPurgeWorker {
            machine_id: "laptop".into(),
            server_id: "primary".into(),
            installation_id: "installation".into(),
            acknowledged: false,
        }],
    })
}

fn snapshot(operations: Vec<TeamOperatorOperation>) -> TeamSnapshot {
    TeamSnapshot {
        control: codex_app_server_protocol::TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 1,
            mode: codex_app_server_protocol::TeamExecutionMode::Paused,
        },
        servers: Vec::new(),
        agents: Vec::new(),
        messages: Vec::new(),
        board: Default::default(),
        rejected_outbox: Vec::new(),
        operations,
    }
}

#[tokio::test]
async fn worker_acknowledgment_changes_cleanup_progress_without_claiming_early_completion() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut operation = purge();
    assert!(operation_summary(&operation).contains("1 worker cleanup pending"));
    let mut snapshot = snapshot(vec![operation.clone()]);
    app.agents_hosts.snapshot = Some(snapshot.clone());
    app.open_agents_operator_operation("purge-42".into());
    let pending = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 54);
    let TeamOperatorOperation::BoardPurge(purge) = &mut operation else {
        unreachable!()
    };
    purge.workers[0].acknowledged = true;
    assert_eq!(
        operation_summary(&operation),
        "Entire shared Board · cleanup complete"
    );
    // A coordinator snapshot updates the open detail view without polling or reopening it.
    snapshot.operations = vec![operation];
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui().unwrap();
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        super::super::agents_hosts::HostUpdate::TeamSnapshot {
            generation: 1,
            snapshot,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        app.agents_hosts.notice.as_deref(),
        Some("Entire shared Board · cleanup complete")
    );
    let complete = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 54);
    insta::assert_snapshot!(
        "board_purge_operation_progress",
        format!("Pending\n{pending}\n\nComplete\n{complete}")
    );
}

#[tokio::test]
async fn snapshot_insertion_keeps_the_selected_operation_identity() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut snapshot = snapshot(vec![purge()]);
    app.agents_hosts.snapshot = Some(snapshot.clone());
    app.open_agents_operator_operations();

    let mut newest = purge();
    let TeamOperatorOperation::BoardPurge(newest_purge) = &mut newest else {
        unreachable!()
    };
    newest_purge.operation_id = "new-operation".into();
    snapshot.operations.push(newest);
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui().unwrap();
    app.handle_agents_host_update(
        &mut tui,
        &mut session,
        super::super::agents_hosts::HostUpdate::TeamSnapshot {
            generation: 1,
            snapshot,
        },
    )
    .await
    .unwrap();

    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(matches!(
        events.try_recv().unwrap(),
        AppEvent::AgentsOperatorOperation(id) if id == "purge-42"
    ));
}
