use super::*;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

fn preview() -> TeamBoardPurgePreview {
    TeamBoardPurgePreview {
        scope: TeamBoardPurgeScope::Topic {
            topic: "test-runs".into(),
        },
        revision: 42,
        conversation_count: 3,
        message_count: 17,
    }
}

#[tokio::test]
async fn topic_actions_do_not_offer_deletion_of_a_hidden_conversation() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    {
        let mut board = app.agents_hosts.board.lock().unwrap();
        board.selected = Some("previous-feed-conversation".into());
        board.topics_open = true;
        board.selected_topic = Some("visible-topic".into());
    }
    app.open_agents_board_actions();
    let rendered = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 72);
    // At this width the shared picker hides its description column, but keeps the disabled marker.
    assert!(rendered.contains("Delete conversation (disabled)"));
    insta::assert_snapshot!("board_purge_topic_actions", rendered);
    app.chat_widget.handle_key_event(KeyCode::Down.into());
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    let AppEvent::AgentsBoardPurge(action) = events.try_recv().unwrap() else {
        panic!("expected visible topic cleanup")
    };
    assert!(matches!(
        *action,
        PurgeAction::Preview(TeamBoardPurgeScope::Topic { topic }) if topic == "visible-topic"
    ));
}

#[tokio::test]
async fn cleanup_confirmation_preserves_reviewed_scope_and_revision() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.chat_widget
        .show_selection_view(purge_confirmation(preview()));
    insta::assert_snapshot!(
        "board_purge_confirmation_narrow",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 48)
    );
    app.chat_widget.handle_key_event(KeyCode::Down.into());
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    let AppEvent::AgentsBoardPurge(action) = events.try_recv().unwrap() else {
        panic!("expected reviewed purge")
    };
    let PurgeAction::Confirm(confirmed) = *action else {
        panic!("expected confirmation")
    };
    assert_eq!(confirmed, preview());
    assert!(app.agents_hosts.team_operations.is_empty());
}

#[tokio::test]
async fn cancelling_cleanup_does_not_submit_any_operation() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.chat_widget
        .show_selection_view(purge_confirmation(preview()));
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(events.try_recv().is_err());
    assert!(app.agents_hosts.team_operations.is_empty());
    assert!(app.chat_widget.no_modal_or_popup_active());
}

#[tokio::test]
async fn empty_scope_cannot_be_submitted() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut empty = preview();
    empty.conversation_count = 0;
    empty.message_count = 0;
    app.handle_agents_board_purge(PurgeAction::Confirm(empty));
    assert!(app.agents_hosts.team_operations.is_empty());
}

#[tokio::test]
async fn cancelled_preview_cannot_open_a_late_confirmation() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = Uuid::new_v4();
    app.agents_hosts.purge_preview_request = Some(id);
    app.handle_agents_board_purge(PurgeAction::CancelPreview(id));
    app.handle_agents_board_purge(PurgeAction::Ready {
        request_id: id,
        result: Ok(preview()),
    });
    assert!(app.chat_widget.no_modal_or_popup_active());
    assert!(app.agents_hosts.team_operations.is_empty());
}
