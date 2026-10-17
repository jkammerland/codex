use super::*;
use pretty_assertions::assert_eq;

#[test]
fn board_submission_retains_a_due_buffered_character_until_ack() {
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let mut composer = ChatComposer::new_with_config(
        /*has_input_focus*/ true,
        AppEventSender::new(tx),
        /*enhanced_keys_supported*/ false,
        "Write a post".into(),
        /*disable_paste_burst*/ false,
        ChatComposerConfig {
            popups_enabled: true,
            ..ChatComposerConfig::plain_text()
        },
    );
    composer.set_board_mentions(&[]);
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "");
    let retained = composer.snapshot_draft_with_pending();
    assert_eq!(retained.text, "x");
    assert_eq!(retained.cursor, 1);
    let (result, _) = composer.handle_submission_with_time(
        /*should_queue*/ false,
        Instant::now() + ChatComposer::recommended_paste_flush_delay(),
    );
    assert_eq!(
        result,
        InputResult::Submitted {
            text: "x".into(),
            text_elements: vec![]
        }
    );
    composer.restore_draft(retained);
    assert_eq!(composer.current_text(), "x");
    assert_eq!(composer.current_cursor(), 1);
}
