use super::*;
use crate::chatwidget::team_execution::TeamExecution;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;

#[tokio::test]
async fn team_hold_status_snapshots_and_draft_never_auto_submits() {
    let (mut chat, _, mut operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.apply_external_edit("Continue after the team resumes".into());

    // Keep the unrelated workspace footer independent of the host's path syntax.
    chat.set_status_line(Some("Test model · project".into()));

    for (name, execution) in [
        ("stopped", TeamExecution::Stopped),
        ("paused", TeamExecution::Paused),
        ("resuming", TeamExecution::Resuming),
        ("syncing", TeamExecution::Syncing),
    ] {
        chat.set_team_execution(Some(execution));
        for width in [32, 96] {
            insta::assert_snapshot!(
                format!("team_{name}_{width}"),
                render_bottom_popup(&chat, width)
            );
        }
        chat.handle_key_event(KeyCode::Enter.into());
        assert_no_submit_op(&mut operations);
    }
    chat.set_team_execution(Some(TeamExecution::Running));
    assert_no_submit_op(&mut operations);
    assert_eq!(
        chat.composer_text_with_pending(),
        "Continue after the team resumes"
    );
}

#[tokio::test]
async fn team_held_start_restores_matching_rich_prompt_and_new_draft_without_draining_queue() {
    let (mut chat, mut events, mut operations) =
        make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    let prompt = UserMessage {
        text: "Inspect [Image #2] @reviewer".into(),
        local_images: vec![LocalImageAttachment {
            placeholder: "[Image #2]".into(),
            path: PathBuf::from("image.png"),
        }],
        remote_image_urls: vec!["https://example.com/image.png".into()],
        text_elements: vec![
            TextElement::new((8..18).into(), Some("[Image #2]".into())),
            TextElement::new((19..28).into(), Some("@reviewer".into())),
        ],
        mention_bindings: vec![MentionBinding {
            sigil: '@',
            mention: "reviewer".into(),
            path: "thread://00000000-0000-0000-0000-000000000001".into(),
        }],
    };
    chat.safety_buffering_prompt = Some(prompt.clone());
    chat.input_queue.user_turn_pending_start = true;
    chat.input_queue.pending_user_message_client_id = Some("start-id".into());
    chat.input_queue
        .queued_user_messages
        .push_back(UserMessage::from("Earlier queue").into());
    chat.apply_external_edit("new draft".into());
    assert!(!chat.restore_team_held_submission("other-id"));
    assert!(chat.restore_team_held_submission("start-id"));
    let draft = chat.bottom_pane.composer_draft_snapshot();
    assert_eq!(
        (
            draft.text,
            draft.local_images,
            draft.remote_image_urls,
            draft.text_elements,
            draft.mention_bindings
        ),
        (
            format!("{}\nnew draft", prompt.text),
            prompt.local_images,
            prompt.remote_image_urls,
            prompt.text_elements,
            prompt.mention_bindings
        )
    );
    assert_eq!(chat.queued_user_message_texts(), vec!["Earlier queue"]);
    assert!(!chat.input_queue.user_turn_pending_start);
    assert_eq!(chat.safety_buffering_prompt, None);
    assert!(!chat.input_queue.suppress_queue_autosend && !chat.input_queue.recovered_queue);
    assert!(!chat.maybe_send_next_queued_input());
    assert_no_submit_op(&mut operations);
    assert!(drain_insert_history(&mut events).iter().any(|lines| {
        lines
            .iter()
            .any(|line| line.to_string().contains("Not sent; draft restored"))
    }));

    // Explicit coordinator confirmation releases only the old queue, never the restored draft.
    chat.set_team_execution(Some(TeamExecution::Running));
    let Op::UserTurn { items, .. } = next_submit_op(&mut operations) else {
        panic!("queued turn")
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "Earlier queue".into(),
            text_elements: Vec::new()
        }]
    );
    assert!(chat.composer_text_with_pending().ends_with("new draft"));
}

#[tokio::test]
async fn team_held_steer_restores_matching_history_without_finishing_active_turn() {
    let (mut chat, _, mut operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "active-turn");
    let mut rejected = pending_steer("wire text");
    rejected.client_id = "rejected".into();
    rejected.history_record = UserMessageHistoryRecord::Override(UserMessageHistoryOverride {
        text: "original command".into(),
        text_elements: Vec::new(),
    });
    let mut retained = pending_steer("accepted earlier");
    retained.client_id = "retained".into();
    chat.input_queue
        .pending_steers
        .extend([retained.clone(), rejected]);
    chat.apply_external_edit("new draft".into());
    assert!(chat.restore_team_held_submission("rejected"));
    assert_eq!(chat.input_queue.pending_steers, VecDeque::from([retained]));
    assert!(chat.turn_lifecycle.agent_turn_running);
    assert_eq!(chat.last_turn_id(), Some("active-turn"));
    assert_eq!(
        chat.composer_text_with_pending(),
        "original command\nnew draft"
    );
    assert_eq!(chat.queued_user_message_texts(), Vec::<String>::new());
    assert_no_submit_op(&mut operations);
}

#[tokio::test]
async fn team_hold_central_submission_and_queue_gates_restore_unsent_input() {
    let (mut chat, _, mut operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_team_execution(Some(TeamExecution::Paused));
    chat.submit_user_message("direct input".into());
    assert!(!chat.queue_user_message("queue input".into()));
    assert_eq!(
        chat.composer_text_with_pending(),
        "queue input\ndirect input"
    );
    assert_eq!(chat.queued_user_message_texts(), Vec::<String>::new());
    assert_no_submit_op(&mut operations);
}

#[tokio::test]
async fn team_hold_during_image_preparation_does_not_permanently_pause_old_queue() {
    let (mut chat, mut events, mut operations) =
        make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    image::RgbImage::new(/*width*/ 2, /*height*/ 2)
        .save(&path)
        .unwrap();
    let message = UserMessage {
        text: "[Image #1] describe".into(),
        local_images: vec![LocalImageAttachment {
            placeholder: "[Image #1]".into(),
            path,
        }],
        remote_image_urls: Vec::new(),
        text_elements: vec![TextElement::new((0..10).into(), Some("[Image #1]".into()))],
        mention_bindings: Vec::new(),
    };
    chat.prepare_image_submission(
        message.clone(),
        UserMessageHistoryRecord::UserMessageText,
        UserMessageSource::Prompt,
    );
    chat.input_queue
        .queued_user_messages
        .push_back(UserMessage::from("earlier queue").into());
    chat.set_team_execution(Some(TeamExecution::Paused));
    loop {
        let event = events.recv().await.expect("image preparation result");
        if let AppEvent::ImagesPrepared(id) = event {
            chat.on_images_prepared(id);
            break;
        }
    }
    assert_eq!(chat.composer_text_with_pending(), message.text);
    assert!(!chat.input_queue.recovered_queue);
    assert_no_submit_op(&mut operations);
    chat.set_team_execution(Some(TeamExecution::Running));
    let Op::UserTurn { items, .. } = next_submit_op(&mut operations) else {
        panic!("queued turn")
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "earlier queue".into(),
            text_elements: Vec::new()
        }]
    );
    assert_eq!(chat.composer_text_with_pending(), message.text);
}

#[tokio::test]
async fn team_held_queue_restore_keeps_colliding_paste_payloads_distinct() {
    let (mut chat, _, mut operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_team_execution(Some(TeamExecution::Paused));
    let existing = "a".repeat(1200);
    let rejected = "b".repeat(1200);
    chat.handle_paste(existing.clone());
    let placeholder = "[Pasted Content 1200 chars]";
    let message = UserMessage {
        text: placeholder.into(),
        text_elements: vec![TextElement::new(
            (0..placeholder.len()).into(),
            Some(placeholder.into()),
        )],
        ..UserMessage::from("")
    };
    assert!(!chat.queue_user_message_with_options(
        message,
        QueuedInputAction::Literal,
        vec![(placeholder.into(), rejected.clone())]
    ));
    let draft = chat.bottom_pane.composer_draft_snapshot();
    let expanded = crate::bottom_pane::ChatComposer::expand_pending_pastes(
        &draft.text,
        draft.text_elements,
        &draft.pending_pastes,
    )
    .0;
    assert_eq!(expanded, format!("{rejected}\n{existing}"));
    assert_no_submit_op(&mut operations);
}

#[tokio::test]
async fn team_hold_blocks_new_voice_from_shortcuts() {
    let (mut chat, mut events, mut operations) =
        make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_team_execution(Some(TeamExecution::Stopped));
    chat.toggle_realtime_conversation();
    assert!(!chat.realtime_conversation_is_running());
    assert!(
        drain_insert_history(&mut events)
            .iter()
            .any(|lines| lines.iter().any(|line| line
                .to_string()
                .contains("resume the team before starting voice")))
    );
    while let Ok(operation) = operations.try_recv() {
        assert!(!matches!(
            operation,
            AppCommand::RealtimeConversationStart { .. }
        ));
    }
}
