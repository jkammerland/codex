use super::super::tests::new_test_composer;
use super::super::tests::snapshot_composer_state_with_width;
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn rewind_enter_and_queue_keys_defer_consuming_the_local_command() {
    for key in [KeyCode::Enter, KeyCode::Tab] {
        for queue_submissions in [false, true] {
            let (mut composer, _rx) = new_test_composer();
            composer.set_text_content("/rewind ".into(), Vec::new(), Vec::new());
            composer.set_queue_submissions(queue_submissions);
            composer.popups.active = ActivePopup::None;

            assert_eq!(
                composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE)),
                (InputResult::Command(SlashCommand::Rewind), true)
            );
            assert_eq!(composer.current_text(), "/rewind ");
            assert!(composer.attachments.is_empty());
        }
    }
}

#[test]
fn rewind_popup_completion_and_enter_are_local() {
    let (mut composer, _rx) = new_test_composer();
    composer.set_text_content("/rewi".into(), Vec::new(), Vec::new());
    assert!(matches!(composer.popups.active, ActivePopup::Command(_)));
    assert_eq!(
        composer.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        (InputResult::None, true)
    );
    assert_eq!(composer.current_text(), "/rewind ");
    assert_eq!(
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        (InputResult::Command(SlashCommand::Rewind), true)
    );

    composer.set_text_content("/rewi".into(), Vec::new(), Vec::new());
    assert_eq!(
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        (InputResult::Command(SlashCommand::Rewind), true)
    );
    assert_eq!(composer.current_text(), "/rewi");

    composer.set_text_content("/rewind".into(), Vec::new(), Vec::new());
    composer.queue_keys = vec![key_hint::ctrl(KeyCode::Char('q'))];
    assert!(matches!(composer.popups.active, ActivePopup::Command(_)));
    assert_eq!(
        composer.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        (InputResult::Command(SlashCommand::Rewind), true)
    );
    assert_eq!(composer.current_text(), "/rewind");
}

#[test]
fn rewind_full_slash_menu_selection_preserves_attachment_validation() {
    for key in [KeyCode::Enter, KeyCode::Tab] {
        for attached in [false, true] {
            let (mut composer, mut rx) = new_test_composer();
            composer.set_text_content("/".into(), Vec::new(), Vec::new());
            if attached {
                composer.set_remote_image_urls(vec!["https://example.com/draft.png".into()]);
            }
            let ActivePopup::Command(popup) = &composer.popups.active else {
                panic!("expected full slash-command menu");
            };
            let rewind_index = popup
                .filtered_items()
                .iter()
                .position(|item| *item == CommandItem::Builtin(SlashCommand::Rewind))
                .expect("rewind must be selectable in the full menu");
            for _ in 0..rewind_index {
                assert_eq!(
                    composer.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                    (InputResult::None, true)
                );
            }
            let ActivePopup::Command(popup) = &composer.popups.active else {
                panic!("expected menu to remain open while navigating");
            };
            assert_eq!(
                popup.selected_item(),
                Some(CommandItem::Builtin(SlashCommand::Rewind))
            );
            let before = composer.draft_snapshot();

            let result = composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE));
            if attached {
                assert_eq!(result, (InputResult::None, true));
                assert_eq!(composer.draft_snapshot(), before);
                assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
            } else if key == KeyCode::Tab {
                assert_eq!(result, (InputResult::None, true));
                assert_eq!(composer.current_text(), "/rewind ");
                assert!(rx.try_recv().is_err());
            } else {
                assert_eq!(result, (InputResult::Command(SlashCommand::Rewind), true));
                assert_eq!(
                    (composer.current_text(), composer.current_cursor()),
                    (before.text, before.cursor)
                );
                assert!(!composer.popup_active());
                assert!(rx.try_recv().is_err());
            }
        }
    }
}

#[test]
fn rewind_rejects_arguments_without_submitting_or_queuing() {
    for text in ["/rewind earlier", "/rewind\nkeep this draft"] {
        for key in [KeyCode::Enter, KeyCode::Tab] {
            let (mut composer, mut rx) = new_test_composer();
            composer.set_text_content(text.into(), Vec::new(), Vec::new());
            composer.set_queue_submissions(/*queue_submissions*/ true);
            let before = composer.draft_snapshot();

            assert_eq!(
                composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE)),
                (InputResult::None, true)
            );
            assert_eq!(composer.draft_snapshot(), before);
            assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
        }
    }
}

#[test]
fn rewind_popup_does_not_discard_a_suffix() {
    for key in [KeyCode::Enter, KeyCode::Tab] {
        let (mut composer, mut rx) = new_test_composer();
        composer.set_text_content("/rewi keep this draft".into(), Vec::new(), Vec::new());
        composer.draft.textarea.set_cursor(/*pos*/ 4);
        composer.sync_popups();
        assert!(matches!(composer.popups.active, ActivePopup::Command(_)));
        let before = composer.draft_snapshot();

        assert_eq!(
            composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE)),
            (InputResult::None, true)
        );
        assert_eq!(composer.draft_snapshot(), before);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
    }
}

#[test]
fn rewind_rejection_retains_attachments_and_pending_pastes() {
    for key in [KeyCode::Enter, KeyCode::Tab] {
        let (mut composer, mut rx) = new_test_composer();
        composer.set_text_content("/rewind ".into(), Vec::new(), Vec::new());
        composer.set_current_cursor(composer.current_text().len());
        composer.attach_image(PathBuf::from("draft.png"));
        composer.set_remote_image_urls(vec!["https://example.com/draft.png".into()]);
        composer.handle_paste("retained ".repeat(LARGE_PASTE_CHAR_THRESHOLD));
        let before = composer.draft_snapshot();

        assert_eq!(
            composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE)),
            (InputResult::None, true)
        );
        assert_eq!(composer.draft_snapshot(), before);
        assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
    }
}

#[test]
fn rewind_rejects_remote_attachment_without_arguments() {
    let (mut composer, mut rx) = new_test_composer();
    composer.set_text_content("/rewind".into(), Vec::new(), Vec::new());
    composer.set_remote_image_urls(vec!["https://example.com/draft.png".into()]);
    let before = composer.draft_snapshot();

    assert_eq!(
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        (InputResult::None, true)
    );
    assert_eq!(composer.draft_snapshot(), before);
    assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
}

#[tokio::test(start_paused = true)]
async fn rewind_buffered_paste_is_local_on_modified_queue_binding() {
    for text in ["/rewind", "/rewind earlier"] {
        for busy in [false, true] {
            let (mut composer, _rx) = new_test_composer();
            composer.queue_keys = vec![key_hint::ctrl(KeyCode::Char('q'))];
            composer.set_queue_submissions(/*queue_submissions*/ true);
            composer.set_task_running(busy);
            let now = tokio::time::Instant::now().into_std();
            for ch in text.chars() {
                composer.draft.paste_burst.append_char_to_buffer(ch, now);
            }
            assert!(composer.current_text().is_empty());
            assert!(composer.is_in_paste_burst());

            let accepted = !busy && text == "/rewind";
            assert_eq!(
                composer.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                (
                    if accepted {
                        InputResult::Command(SlashCommand::Rewind)
                    } else {
                        InputResult::None
                    },
                    true
                )
            );
            assert_eq!(composer.current_text(), text);
            assert!(!composer.is_in_paste_burst());
        }
    }
}

#[test]
fn rewind_hidden_in_a_pending_paste_cannot_become_a_model_prompt() {
    let (mut composer, mut rx) = new_test_composer();
    composer.handle_paste(format!(
        "/rewind {}",
        "x".repeat(LARGE_PASTE_CHAR_THRESHOLD)
    ));
    let before = composer.draft_snapshot();

    assert_eq!(
        composer.handle_submission(/*should_queue*/ true),
        (InputResult::None, true)
    );
    assert_eq!(composer.draft_snapshot(), before);
    assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
}

#[test]
fn rewind_busy_parent_owned_and_side_threads_retain_the_command() {
    let states: [fn(&mut ChatComposer); 4] = [
        |composer: &mut ChatComposer| composer.set_task_running(/*running*/ true),
        |composer: &mut ChatComposer| composer.set_team_execution_held(/*held*/ true),
        ChatComposer::set_parent_owned_thread,
        |composer: &mut ChatComposer| composer.set_side_conversation_active(/*active*/ true),
    ];
    for setup in states {
        for key in [KeyCode::Enter, KeyCode::Tab] {
            let (mut composer, mut rx) = new_test_composer();
            setup(&mut composer);
            composer.set_text_content("/rewind".into(), Vec::new(), Vec::new());
            let before = composer.draft_snapshot();

            assert_eq!(
                composer.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE)),
                (InputResult::None, true)
            );
            assert_eq!(composer.draft_snapshot(), before);
            assert!(matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_))));
        }
    }
}

#[test]
fn rewind_offline_and_unavailable_submission_preserves_the_full_draft() {
    for mode in [
        RestrictedInputMode::Disconnected,
        RestrictedInputMode::UnavailableThread,
    ] {
        for key in [KeyCode::Enter, KeyCode::Tab] {
            let (mut composer, mut rx) = new_test_composer();
            composer.set_text_content("/rewind ".into(), Vec::new(), Vec::new());
            composer.set_current_cursor(composer.current_text().len());
            composer.handle_paste("retained ".repeat(LARGE_PASTE_CHAR_THRESHOLD));
            let before = composer.draft_snapshot();

            assert_eq!(
                composer.handle_restricted_key(KeyEvent::new(key, KeyModifiers::NONE), mode),
                InputResult::None
            );
            assert_eq!(composer.draft_snapshot(), before);
            assert!(rx.try_recv().is_err());
        }
    }
}

#[test]
fn rewind_completion_popup_snapshot() {
    snapshot_composer_state_with_width(
        "rewind_completion_popup",
        /*width*/ 80,
        /*enhanced_keys_supported*/ false,
        |composer| composer.set_text_content("/rewi".into(), Vec::new(), Vec::new()),
    );
}
