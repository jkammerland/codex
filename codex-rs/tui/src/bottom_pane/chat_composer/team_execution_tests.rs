use super::super::tests::new_test_composer;
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn team_hold_preserves_rich_draft_history_and_custom_submit_or_queue() {
    let (mut composer, _) = new_test_composer();
    composer.set_disable_paste_burst(/*disabled*/ true);
    composer.handle_paste("x".repeat(1200));
    composer.attach_image(PathBuf::from("image.png"));
    composer.insert_selected_mention(
        0..0,
        "@reviewer",
        Some("thread://00000000-0000-0000-0000-000000000001"),
    );
    let original = composer.snapshot_draft();
    let mut keymap = RuntimeKeymap::defaults();
    keymap.composer.submit = vec![key_hint::plain(KeyCode::F(2))];
    composer.set_keymap_bindings(&keymap);
    composer.set_team_execution_held(true);
    composer.set_task_running(true);
    for key in [KeyCode::F(2), KeyCode::Tab] {
        assert_eq!(composer.handle_key_event(key.into()).0, InputResult::None);
        assert_eq!(composer.snapshot_draft(), original);
    }
    // Held input never enters recall history, but ordinary editor input still works.
    composer.set_text_content(String::new(), Vec::new(), Vec::new());
    composer.handle_key_event(KeyCode::Up.into());
    assert_eq!(composer.current_text(), "");
    composer.handle_key_event(KeyCode::Char('z').into());
    assert_eq!(composer.current_text(), "z");
}

#[test]
fn team_hold_blocks_popup_and_inline_prompt_dispatch_but_allows_local_commands() {
    let (mut composer, _) = new_test_composer();
    composer.set_team_execution_held(true);
    composer.set_collaboration_modes_enabled(true);
    composer.set_goal_command_enabled(true);
    composer.set_voice_command_enabled(true);
    for draft in [
        "/review",
        "/plan investigate",
        "/goal status",
        "/voice",
        "/voice start",
        "plain prompt",
    ] {
        composer.set_text_content(draft.into(), Vec::new(), Vec::new());
        composer.sync_popups();
        let original = composer.snapshot_draft();
        assert_eq!(
            composer.handle_key_event(KeyCode::Enter.into()).0,
            InputResult::None
        );
        assert_eq!(composer.snapshot_draft(), original);
        assert!(composer.pending_slash_command_history.is_none());
    }
    composer.set_text_content("/agents".into(), Vec::new(), Vec::new());
    composer.sync_popups();
    assert_eq!(
        composer.handle_key_event(KeyCode::Enter.into()).0,
        InputResult::Command(SlashCommand::Agents)
    );
}

#[test]
fn team_hold_keeps_explicit_voice_controls_available() {
    let (mut composer, _) = new_test_composer();
    composer.set_team_execution_held(true);
    composer.set_voice_command_enabled(true);
    for action in ["settings", "mute", "stop"] {
        composer.set_text_content(format!("/voice {action}"), Vec::new(), Vec::new());
        composer.sync_popups();
        assert_eq!(
            composer.handle_key_event(KeyCode::Enter.into()).0,
            InputResult::CommandWithArgs(SlashCommand::Voice, action.into(), Vec::new())
        );
    }
}
