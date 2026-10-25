use super::*;
use crate::app::owned_transcript::tests::attach_thread;
use crate::app::owned_transcript::tests::user_cell;
use crate::app::tests::make_test_app_with_channels;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, Debug)]
enum Unavailable {
    Draft,
    Attachment,
    Offline,
    Reader,
    MissingThread,
    Running,
    EmptyHistory,
}

fn make_unavailable(app: &mut App, thread_id: ThreadId, state: Unavailable) {
    match state {
        Unavailable::Draft => app
            .chat_widget
            .apply_external_edit("Keep this draft".into()),
        Unavailable::Attachment => app
            .chat_widget
            .set_remote_image_urls(vec!["https://example.com/image.png".into()]),
        Unavailable::Offline => app.reconnect.offline = true,
        Unavailable::Reader => app.chat_widget.show_external_writer_thread(),
        Unavailable::EmptyHistory => app.transcript_cells.clear(),
        Unavailable::MissingThread => {
            app.ensure_thread_channel(thread_id).mark_replay_only();
            app.chat_widget.pause_unavailable_thread();
        }
        Unavailable::Running => app.chat_widget.handle_server_notification(
            ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: thread_id.to_string(),
                turn: Turn {
                    id: "new-turn".into(),
                    root_turn_id: None,
                    items_view: TurnItemsView::Full,
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            }),
            /*replay_kind*/ None,
        ),
    }
}

#[tokio::test]
async fn rewind_action_preserves_unavailable_conversations() -> Result<()> {
    for state in [
        Unavailable::Draft,
        Unavailable::Attachment,
        Unavailable::Offline,
        Unavailable::Reader,
        Unavailable::MissingThread,
        Unavailable::Running,
        Unavailable::EmptyHistory,
    ] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        let thread_id = ThreadId::new();
        attach_thread(&mut app, thread_id);
        app.app_server_target = AppServerTarget::Remote {
            endpoint: crate::resolve_remote_addr("ws://127.0.0.1:4500")?,
        };
        app.transcript_cells = vec![user_cell("Earlier prompt")];
        make_unavailable(&mut app, thread_id, state);
        let draft = app.chat_widget.composer_text_with_pending();
        let images = app.chat_widget.remote_image_urls().to_vec();
        while events.try_recv().is_ok() {}
        let mut server = AppServerSession::discovery_placeholder();
        let mut tui = crate::tui::test_support::make_test_tui()?;

        app.handle_event(&mut tui, &mut server, AppEvent::OpenRewind)
            .await?;

        assert_eq!(
            (app.backtrack.overlay_preview_active, app.overlay.is_some()),
            (false, false),
            "{state:?}"
        );
        assert_eq!(
            app.chat_widget.composer_text_with_pending(),
            draft,
            "{state:?}"
        );
        assert_eq!(app.chat_widget.remote_image_urls(), images, "{state:?}");
        let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            emitted
                .iter()
                .any(|event| matches!(event, AppEvent::InsertHistoryCell(_))),
            "missing local explanation: {state:?}"
        );
        assert!(
            !emitted.iter().any(|event| matches!(
                event,
                AppEvent::RevertSessionForPromptEdit { .. } | AppEvent::CodexOp(_)
            )),
            "{state:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn rewind_confirmation_rechecks_changed_conversation() -> Result<()> {
    for owned in [true, false] {
        for state in [
            Unavailable::Offline,
            Unavailable::MissingThread,
            Unavailable::Running,
            Unavailable::Draft,
            Unavailable::Attachment,
        ] {
            let (mut app, mut events, _) = make_test_app_with_channels().await;
            let thread_id = ThreadId::new();
            attach_thread(&mut app, thread_id);
            app.app_server_target = AppServerTarget::Remote {
                endpoint: crate::resolve_remote_addr("ws://127.0.0.1:4500")?,
            };
            app.transcript_cells = vec![user_cell("Earlier prompt")];
            let mut server = AppServerSession::discovery_placeholder();
            let mut tui = crate::tui::test_support::make_test_tui()?;
            tui.set_owned_screen(owned)?;
            app.handle_event(&mut tui, &mut server, AppEvent::OpenRewind)
                .await?;
            assert!(app.backtrack.overlay_preview_active);
            make_unavailable(&mut app, thread_id, state);
            let draft = app.chat_widget.composer_text_with_pending();
            let images = app.chat_widget.remote_image_urls().to_vec();
            while events.try_recv().is_ok() {}

            app.handle_tui_event(&mut tui, &mut server, TuiEvent::Key(KeyCode::Enter.into()))
                .await?;

            assert!(
                app.backtrack.overlay_preview_active,
                "owned={owned}, {state:?}"
            );
            assert_eq!(
                app.chat_widget.composer_text_with_pending(),
                draft,
                "owned={owned}, {state:?}"
            );
            assert_eq!(
                app.chat_widget.remote_image_urls(),
                images,
                "owned={owned}, {state:?}"
            );
            assert!(
                !std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
                    event,
                    AppEvent::RevertSessionForPromptEdit { .. } | AppEvent::CodexOp(_)
                )),
                "owned={owned}, {state:?}"
            );
            tui.set_owned_screen(/*owned*/ false)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn rewind_action_escape_cancels_without_leaving_for_agents() -> Result<()> {
    for owned in [true, false] {
        let (mut app, mut events, _) = make_test_app_with_channels().await;
        attach_thread(&mut app, ThreadId::new());
        app.agents_hosts.manifest = Some(PathBuf::from("test-manifest.toml"));
        app.transcript_cells = vec![user_cell("Earlier prompt")];
        let mut server = AppServerSession::discovery_placeholder();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(owned)?;
        app.handle_event(&mut tui, &mut server, AppEvent::OpenRewind)
            .await?;
        assert!(app.backtrack.overlay_preview_active);

        app.handle_tui_event(&mut tui, &mut server, TuiEvent::Key(KeyCode::Esc.into()))
            .await?;

        assert_eq!(
            (
                app.backtrack.overlay_preview_active,
                app.overlay.is_some(),
                app.chat_widget.has_active_view()
            ),
            (false, false, false)
        );
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
                event,
                AppEvent::RevertSessionForPromptEdit { .. } | AppEvent::OpenAgentsOverview
            ))
        );

        // Legacy inline transcript editing permits a draft; the explicit marker must not leak.
        tui.set_owned_screen(/*owned*/ false)?;
        app.agents_hosts.manifest = None;
        app.chat_widget.apply_external_edit("legacy draft".into());
        app.open_transcript_overlay(&mut tui);
        for key in [KeyCode::Esc, KeyCode::Enter] {
            app.handle_tui_event(&mut tui, &mut server, TuiEvent::Key(key.into()))
                .await?;
        }
        assert_eq!(
            std::iter::from_fn(|| events.try_recv().ok())
                .filter(|event| matches!(event, AppEvent::RevertSessionForPromptEdit { .. }))
                .count(),
            1
        );
        tui.set_owned_screen(/*owned*/ false)?;
    }
    Ok(())
}
