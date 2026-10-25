//! A queued older page must remain consistent with an asynchronous prompt revert.

use super::pagination_completion_tests::completed_history_app;
use super::*;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy)]
enum PageArrival {
    BeforeResponse,
    BeforeTrim,
    AfterTrim,
    RequestedAfterTrim,
    ReattachedBeforeResponse,
}

#[tokio::test]
async fn prompt_revert_pagination_before_response_keeps_loaded_prefix() -> Result<()> {
    boxed_future!(prompt_revert_pagination_scenario(
        PageArrival::BeforeResponse,
        /*selected*/ 1
    ))
    .await
}

#[tokio::test]
async fn prompt_revert_pagination_before_trim_keeps_selected_identity() -> Result<()> {
    boxed_future!(prompt_revert_pagination_scenario(
        PageArrival::BeforeTrim,
        /*selected*/ 1
    ))
    .await
}

#[tokio::test]
async fn prompt_revert_pagination_after_trim_keeps_pending_page() -> Result<()> {
    boxed_future!(prompt_revert_pagination_scenario(
        PageArrival::AfterTrim,
        /*selected*/ 1
    ))
    .await
}

#[tokio::test]
async fn prompt_revert_pagination_first_visible_prompt_can_load_retained_history() -> Result<()> {
    boxed_future!(prompt_revert_pagination_scenario(
        PageArrival::RequestedAfterTrim,
        /*selected*/ 0
    ))
    .await
}

#[tokio::test]
async fn prompt_revert_pagination_reattached_before_response_trims_rebuilt_transcript() -> Result<()>
{
    boxed_future!(prompt_revert_pagination_scenario(
        PageArrival::ReattachedBeforeResponse,
        /*selected*/ 1
    ))
    .await
}

async fn prompt_revert_pagination_scenario(arrival: PageArrival, selected: usize) -> Result<()> {
    let (mut app, _codex_home, thread_id) =
        completed_history_app(&["Oldest", "Older", "Retained", "Selected"]).await?;
    app.local_settings.tui.terminal_resize_reflow_max_rows = Some(4);
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    app.app_event_tx = crate::app_event_sender::AppEventSender::new(event_tx);
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let started = server
        .resume_thread(
            &app.local_settings,
            app.config.clone(),
            thread_id,
            crate::app_server_session::ResumeModelSettings::RestoreFromThread,
        )
        .await?;
    app.transcript_cells = crate::thread_transcript::thread_items_to_transcript_cells(
        Some(thread_id),
        &app.config.cwd,
        started.turns.iter().flat_map(|turn| turn.items.clone()),
        crate::thread_transcript::RawReasoningVisibility::Hidden,
        Some(&app.config),
    );
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    app.scrollback_has_older_history = server.has_older_history(thread_id);
    assert_eq!(
        visible_prompts(&app),
        vec!["Retained prompt", "Selected prompt"]
    );
    let selected_cell = Arc::clone(
        &app.transcript_cells[nth_user_position(&app.transcript_cells, selected).unwrap()],
    );
    let prompt = visible_prompts(&app)[selected].to_string();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;

    let other_thread_id = if matches!(arrival, PageArrival::ReattachedBeforeResponse) {
        let other = server.start_thread(&app.config).await?;
        let id = other.session.thread_id;
        app.ensure_thread_channel(id)
            .store
            .lock()
            .await
            .set_session(other.session, other.turns);
        Some(id)
    } else {
        None
    };

    // Hold the actual bounded page response until the chosen dispatcher boundary.
    let mut held_page = if matches!(arrival, PageArrival::RequestedAfterTrim) {
        None
    } else {
        let cursor = server
            .begin_older_history_page(thread_id)
            .expect("older page");
        let page = server
            .thread_items_page(
                thread_id,
                /*turn_id*/ None,
                Some(cursor.clone()),
                /*limit*/ 100,
            )
            .await?;
        Some((cursor, page))
    };
    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::RevertSessionForPromptEdit {
            thread_id,
            selected_cell,
            prompt: prompt.clone().into(),
        },
    ))
    .await?;
    assert!(app.pending_host_prompt_reverts.contains_key(&thread_id));
    if matches!(arrival, PageArrival::BeforeResponse) {
        let (cursor, page) = held_page.take().expect("held older page");
        app.handle_older_history_page(&mut tui, &mut server, thread_id, &cursor, Ok(page))
            .await?;
    }

    // Drive the real response and notification without sleeps or a simulated backend.
    loop {
        tokio::select! {
            event = events.recv() => {
                let event = event.expect("prompt revert app event");
                if let AppEvent::AgentsHostUpdate(update) = &event
                    && let agents_hosts::HostUpdate::PromptRevertPrepared { result, .. } = update.as_ref()
                {
                    assert!(result.is_ok(), "the backend revert must succeed");
                    if let Some(other_thread_id) = other_thread_id {
                        // Hold the real completion while actual thread selection rebuilds the UI.
                        // Both switches replay cached stores, as navigation does before dispatch.
                        for selected_thread_id in [other_thread_id, thread_id] {
                            Box::pin(app.handle_event(
                                &mut tui,
                                &mut server,
                                AppEvent::SelectAgentThread(selected_thread_id),
                            )).await?;
                            while let Ok(queued) = events.try_recv() {
                                Box::pin(app.handle_event(&mut tui, &mut server, queued)).await?;
                            }
                            assert_eq!(app.chat_widget.thread_id(), Some(selected_thread_id));
                        }
                        assert_eq!(visible_prompts(&app), vec!["Retained prompt", "Selected prompt"]);
                        let pending = &app.pending_host_prompt_reverts[&thread_id];
                        assert!(app.transcript_cells.iter().all(|cell| !Arc::ptr_eq(cell, &pending.selected_cell)));
                    }
                }
                let finish = matches!(event, AppEvent::FinishPromptRevert { .. });
                if finish && matches!(arrival, PageArrival::BeforeTrim) {
                    let (cursor, page) = held_page.take().expect("held older page");
                    app.handle_older_history_page(&mut tui, &mut server, thread_id, &cursor, Ok(page))
                        .await?;
                }
                let restored_input = finish.then(|| app.chat_widget.capture_thread_input_state());
                Box::pin(app.handle_event(&mut tui, &mut server, event)).await?;
                if finish {
                    // Reattachment recovery uses the ordinary queued reset/replay barrier.
                    while let Ok(queued) = events.try_recv() {
                        Box::pin(app.handle_event(&mut tui, &mut server, queued)).await?;
                    }
                    assert_eq!(Some(app.chat_widget.capture_thread_input_state()), restored_input);
                    assert!(app.chat_widget.queued_user_message_texts().is_empty());
                    break;
                }
            }
            event = server.next_event() => {
                app.handle_backend_event(&server, event.expect("embedded backend event")).await;
            }
        }
    }
    if matches!(arrival, PageArrival::RequestedAfterTrim) {
        let cursor = server
            .begin_older_history_page(thread_id)
            .expect("retained older history");
        let page = server
            .thread_items_page(
                thread_id,
                /*turn_id*/ None,
                Some(cursor.clone()),
                /*limit*/ 100,
            )
            .await?;
        held_page = Some((cursor, page));
    }
    if let Some((cursor, page)) = held_page {
        app.handle_older_history_page(&mut tui, &mut server, thread_id, &cursor, Ok(page))
            .await?;
    }

    // Compare all three representations, including the pagination end state.
    let backend = server
        .thread_read(thread_id, /*include_turns*/ true)
        .await?;
    let replay = app.thread_event_channels[&thread_id]
        .store
        .lock()
        .await
        .turns
        .clone();
    let expected = if selected == 0 {
        vec!["Oldest prompt", "Older prompt"]
    } else {
        vec!["Oldest prompt", "Older prompt", "Retained prompt"]
    };
    assert_eq!(
        (
            visible_prompts(&app),
            turn_prompts(&replay),
            turn_prompts(&backend.turns),
            server.has_older_history(thread_id),
            app.scrollback_has_older_history,
            app.pending_thread_switch_resets,
            app.chat_widget.composer_text_with_pending(),
        ),
        (
            expected.clone(),
            expected.clone(),
            expected,
            false,
            false,
            0,
            prompt
        ),
    );
    tui.set_owned_screen(/*owned*/ false)?;
    server.shutdown().await?;
    Ok(())
}

fn visible_prompts(app: &App) -> Vec<&str> {
    app.transcript_cells
        .iter()
        .filter_map(|cell| {
            cell.as_any()
                .downcast_ref::<crate::history_cell::UserHistoryCell>()
        })
        .map(|cell| cell.message.as_str())
        .collect()
}

fn turn_prompts(turns: &[Turn]) -> Vec<&str> {
    turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| {
            let ThreadItem::UserMessage { content, .. } = item else {
                return None;
            };
            content.iter().find_map(|input| match input {
                codex_app_server_protocol::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
        })
        .collect()
}
