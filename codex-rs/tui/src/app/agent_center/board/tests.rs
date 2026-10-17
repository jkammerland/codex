use super::*;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::ChatComposerConfig;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[path = "navigation_tests.rs"]
mod navigation_tests;

fn address() -> TeamAddress {
    TeamAddress {
        machine_id: "laptop".into(),
        server_id: "primary".into(),
        thread_id: "reviewer-task".into(),
    }
}

fn entry(id: &str, root: &str, seq: i64, body: &str, user: bool) -> TeamBoardEntry {
    TeamBoardEntry {
        message: TeamMessage {
            id: id.into(),
            sender: address(),
            recipient: None,
            kind: TeamMessageKind::Board,
            body: body.into(),
            topic: Some("release".into()),
            reply_to: (id != root).then(|| root.into()),
            created_at: 1_700_000_000 + seq,
        },
        conversation_id: root.into(),
        append_seq: seq,
        author: TeamBoardAuthor {
            identity: if user {
                TeamBoardIdentity::User
            } else {
                TeamBoardIdentity::Agent { address: address() }
            },
            name: if user { "User" } else { "Reviewer" }.into(),
            machine_name: (!user).then(|| "MacBook".into()),
            legacy: false,
        },
        mentions: vec![],
        notifications: vec![],
    }
}

fn fixture() -> (BoardView, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sender = AppEventSender::new(tx);
    let root = entry(
        "root",
        "root",
        1,
        "Ready for the release review?\nPlease check the native build and share any blockers here.",
        true,
    );
    let reply = entry(
        "reply",
        "root",
        2,
        "The Mac build passes. I am reviewing the asynchronous delivery path next.",
        false,
    );
    let members = vec![
        TeamBoardMember {
            author: root.author.clone(),
            available: true,
        },
        TeamBoardMember {
            author: reply.author.clone(),
            available: false,
        },
    ];
    let mut composer = ChatComposer::new_with_config(
        true,
        sender.clone(),
        false,
        "Write a post or reply · @ mention an agent".into(),
        true,
        ChatComposerConfig {
            popups_enabled: true,
            trim_submission: false,
            ..ChatComposerConfig::plain_text()
        },
    );
    composer.set_board_mentions(&members);
    let state = BoardState {
        active: true,
        composer: Some(composer),
        selected: Some("root".into()),
        snapshot: TeamBoardSnapshot {
            revision: 4,
            entries: vec![root.clone(), reply.clone()],
            followers: vec![
                TeamBoardFollower {
                    conversation_id: "root".into(),
                    identity: TeamBoardIdentity::User,
                    author: root.author,
                },
                TeamBoardFollower {
                    conversation_id: "root".into(),
                    identity: reply.author.identity.clone(),
                    author: reply.author,
                },
            ],
            members,
            ..Default::default()
        },
        ..Default::default()
    };
    (
        BoardView {
            state: Arc::new(Mutex::new(state)),
            sender,
        },
        rx,
    )
}

fn key(view: &mut BoardView, code: KeyCode) {
    view.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
}

fn render(view: &BoardView, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    buffer
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn board_team_control_status_updates_preserve_composer_and_geometry() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Keep this draft".into());
    let draft = view.state().composer.as_ref().unwrap().snapshot_draft();
    let mut snapshot = codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 10,
            mode: TeamExecutionMode::Running,
        },
        servers: vec![],
        agents: vec![],
        messages: vec![],
        board: view.state().snapshot.clone(),
        rejected_outbox: vec![],
    };
    let mut frames = Vec::new();
    for (mode, revision, applied, total) in [
        (TeamExecutionMode::Running, 10, 3, 3),
        (TeamExecutionMode::Paused, 11, 1, 3),
        (TeamExecutionMode::Paused, 11, 3, 3),
        (TeamExecutionMode::Stopped, 12, 3, 3),
        (TeamExecutionMode::Stopped, 12, 100, 100),
    ] {
        snapshot.control.mode = mode;
        snapshot.control.revision = revision;
        snapshot.servers = (0..total)
            .map(|index| codex_team::ServerStatus {
                machine_id: format!("machine-{index}"),
                machine_label: format!("Machine {index}"),
                server_id: "primary".into(),
                server_label: "Primary".into(),
                installation_id: None,
                connected: true,
                applied_revision: Some(if index < applied {
                    revision
                } else {
                    revision - 1
                }),
                error: None,
            })
            .collect();
        view.state().update_team(&snapshot);
        let frame = render(&view, 40, 12);
        assert_eq!(
            view.state().composer.as_ref().unwrap().snapshot_draft(),
            draft,
            "control-only updates must retain the exact draft"
        );
        frames.push(frame);
    }
    insta::assert_snapshot!("board_control_modes_narrow", frames.join("\n\n"));
    insta::assert_snapshot!("board_control_stopped_wide", render(&view, 120, 12));
}

#[test]
fn board_feed_conversation_followers_and_short_terminal() {
    let (mut view, _) = fixture();
    insta::assert_snapshot!("board_feed_wide", render(&view, 120, 22));
    assert!(
        view.state().displayed.is_none(),
        "preview must not record read exposure"
    );
    key(&mut view, KeyCode::Enter);
    insta::assert_snapshot!("board_conversation_narrow", render(&view, 70, 19));
    insta::assert_snapshot!("board_very_short", render(&view, 40, 3));
    key(&mut view, KeyCode::Char('v'));
    insta::assert_snapshot!("board_followers", render(&view, 90, 12));
}

#[test]
fn board_fuzzy_mentions_are_named_atomic_and_never_search_files() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    key(&mut view, KeyCode::Char('@'));
    assert!(view.state().composer.as_ref().unwrap().popup_active());
    for ch in "mcbk".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    insta::assert_snapshot!("board_mention_picker", render(&view, 100, 20));
    key(&mut view, KeyCode::Enter);
    assert!(
        events.try_recv().is_err(),
        "mention acceptance must not send or search"
    );
    let state = view.state();
    let composer = state.composer.as_ref().unwrap();
    assert_eq!(
        composer
            .mention_bindings()
            .iter()
            .map(|binding| (&binding.mention, &binding.path))
            .collect::<Vec<_>>(),
        vec![(
            &"Reviewer".to_string(),
            &format!(
                "board:{}",
                serde_json::to_string(&TeamBoardIdentity::Agent { address: address() }).unwrap()
            )
        )]
    );
    drop(state);
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("expected Board send")
    };
    let TeamRequest::Board {
        request: TeamBoardParams::Post { mentions, body, .. },
    } = operation.request
    else {
        panic!("expected post")
    };
    assert_eq!(
        mentions,
        vec![TeamBoardMention {
            identity: TeamBoardIdentity::Agent { address: address() },
            display: "@Reviewer".into(),
            start: 0,
            end: 9
        }]
    );
    assert_eq!(body, "@Reviewer ");
}

#[test]
fn escape_closes_mention_before_leaving_draft_and_letters_stay_text() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Char('n'));
    for ch in "nrflt".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .current_text_with_pending(),
        "nrflt"
    );
    key(&mut view, KeyCode::Char(' '));
    key(&mut view, KeyCode::Char('@'));
    key(&mut view, KeyCode::Esc);
    assert!(view.state().composing.is_some());
    assert!(!view.state().composer.as_ref().unwrap().popup_active());
    key(&mut view, KeyCode::Esc);
    assert!(view.state().composing.is_none());
    key(&mut view, KeyCode::Char('n'));
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .current_text_with_pending(),
        "nrflt @"
    );
}

#[test]
fn only_successful_focused_contiguous_frames_advance_read_cursor() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Enter);
    render(&view, 70, 19);
    assert!(view.state().read_requested.is_empty());
    assert_eq!(view.state().take_frame_read(false), None);
    render(&view, 70, 19);
    assert_eq!(view.state().take_frame_read(true), Some(("root".into(), 2)));
    render(&view, 70, 19);
    assert_eq!(view.state().take_frame_read(true), None);
    assert_eq!(view.state().unread_boundary.get("root"), Some(&0));
}

#[test]
fn jumping_past_unseen_long_entry_does_not_mark_it_read() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries[0].message.body =
        (0..80).map(|n| format!("line {n}\n")).collect();
    key(&mut view, KeyCode::Enter);
    key(&mut view, KeyCode::End);
    render(&view, 70, 12);
    assert_eq!(view.state().take_frame_read(true), None);
    key(&mut view, KeyCode::Home);
    for _ in 0..20 {
        render(&view, 70, 12);
        view.state().take_frame_read(true);
        key(&mut view, KeyCode::PageDown);
    }
    assert_eq!(view.state().read_requested.get("root"), Some(&2));
}

#[test]
fn acknowledgments_keep_edits_and_retry_original_operation() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Original post".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("send missing")
    };
    view.state()
        .operation_finished(&operation.operation_id, false);
    view.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsTeamRetry(id) if id == operation.operation_id)
    );
    key(&mut view, KeyCode::Char('!'));
    view.state()
        .operation_finished(&operation.operation_id, true);
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .current_text_with_pending(),
        "Original post!"
    );
    assert!(view.state().composing.is_some());
}

#[test]
fn selection_and_drafts_survive_new_replies_and_stale_snapshots() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Char('r'));
    view.handle_paste("Keep my draft".into());
    let mut snapshot = view.state().snapshot.clone();
    let stale = snapshot.clone();
    snapshot.revision += 1;
    snapshot
        .entries
        .push(entry("other", "other", 3, "Another topic", false));
    view.state().update(snapshot);
    view.state().update(stale);
    assert_eq!(view.state().selected.as_deref(), Some("root"));
    assert_eq!(view.state().snapshot.revision, 5);
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .current_text_with_pending(),
        "Keep my draft"
    );
}

#[test]
fn departed_mention_retains_identity_and_blocks_send() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("@Reviewer".into());
    key(&mut view, KeyCode::Tab);
    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot
        .members
        .retain(|member| member.author.identity == TeamBoardIdentity::User);
    view.state().update(snapshot);
    key(&mut view, KeyCode::Enter);
    assert!(events.try_recv().is_err());
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .mention_bindings()
            .len(),
        1
    );
    assert!(
        view.state()
            .notice
            .as_deref()
            .unwrap()
            .contains("left the team")
    );
}

#[test]
fn unselected_plain_at_text_never_routes_a_mention() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Please ask @Reviewer later".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("send missing")
    };
    assert!(
        matches!(operation.request, TeamRequest::Board { request: TeamBoardParams::Post { mentions, .. } } if mentions.is_empty())
    );
}

#[test]
fn human_follow_and_reply_to_entry_use_explicit_identities() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('f'));
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsTeamRequest(TeamRequest::Board { request: TeamBoardParams::Follow { conversation_id, identity: None, enabled: false } }) if conversation_id == "root")
    );
    key(&mut view, KeyCode::Enter);
    key(&mut view, KeyCode::Char(']'));
    key(&mut view, KeyCode::Char('r'));
    view.handle_paste("Thank you".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("send missing")
    };
    assert!(
        matches!(operation.request, TeamRequest::Board { request: TeamBoardParams::Reply { conversation_id, reply_to: Some(reply_to), .. } } if conversation_id == "root" && reply_to == "reply")
    );
}

#[test]
fn searching_selects_a_visible_conversation_before_opening() {
    let (mut view, _) = fixture();
    view.state()
        .snapshot
        .entries
        .push(entry("other", "other", 3, "Compiler question", false));
    key(&mut view, KeyCode::Char('/'));
    for ch in "Compiler".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    key(&mut view, KeyCode::Enter);
    key(&mut view, KeyCode::Enter);
    assert_eq!(view.state().opened.as_deref(), Some("other"));
    assert_eq!(view.selected_index(), Some(usize::MAX));
}

#[test]
fn cursor_movement_does_not_keep_an_acknowledged_draft() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Post this".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("send missing")
    };
    key(&mut view, KeyCode::Left);
    view.state()
        .operation_finished(&operation.operation_id, true);
    assert!(view.state().composing.is_none());
    assert_eq!(view.state().composer.as_ref().unwrap().current_text(), "");
}

#[test]
fn tab_without_a_mention_does_not_submit() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Draft".into());
    key(&mut view, KeyCode::Tab);
    assert!(events.try_recv().is_err());
    assert!(view.state().composing.is_some());
}

#[test]
fn conversation_preserves_paragraphs_in_multiline_posts() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries[0].message.body = "First paragraph.\n\nSecond paragraph.".into();
    key(&mut view, KeyCode::Enter);
    let output = render(&view, 70, 19);
    assert!(
        output.contains("First paragraph.\n\nSecond paragraph."),
        "{output}"
    );
}

#[test]
fn unread_ignores_own_entries_without_hiding_unread_agent_replies() {
    let (view, _) = fixture();
    let mut state = view.state();
    assert!(state.unread("root"));
    state
        .snapshot
        .entries
        .push(entry("own-reply", "root", 3, "Thanks", true));
    assert!(state.unread("root"));
    state.snapshot.reads.push(TeamBoardReadState {
        conversation_id: "root".into(),
        identity: TeamBoardIdentity::User,
        append_seq: 2,
    });
    assert!(!state.unread("root"));
    assert!(!state.unread("missing"));
}

#[test]
fn blank_viewport_is_not_read_credit_for_a_long_incoming_reply() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries.truncate(1);
    view.state().snapshot.entries[0].message.body = "Short original post".into();
    key(&mut view, KeyCode::Enter);
    render(&view, 70, 20);
    assert_eq!(view.state().take_frame_read(true), Some(("root".into(), 1)));
    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot.entries.push(entry(
        "long-reply",
        "root",
        2,
        &(0..16).map(|n| format!("line {n}\n")).collect::<String>(),
        false,
    ));
    view.state().update(snapshot);
    render(&view, 70, 20);
    assert_eq!(view.state().take_frame_read(true), None);
    assert_eq!(view.state().read_requested.get("root"), Some(&1));
}

#[test]
fn failed_read_ack_retries_its_original_operation_when_reopened() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Enter);
    render(&view, 70, 19);
    assert_eq!(view.state().take_frame_read(true), Some(("root".into(), 2)));
    view.state().pending_reads.insert(
        "same-operation".into(),
        PendingRead {
            conversation: "root".into(),
            append_seq: 2,
            failed: false,
            confirmed: false,
        },
    );
    assert!(!view.state().operation_finished("same-operation", false));
    assert!(view.state().read_requested.is_empty());
    render(&view, 70, 19);
    assert_eq!(
        view.state().take_frame_read(true),
        None,
        "redraw must not start a retry loop"
    );
    key(&mut view, KeyCode::Esc);
    key(&mut view, KeyCode::Enter);
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsTeamRetry(id) if id == "same-operation")
    );
    assert!(view.state().operation_finished("same-operation", true));
    assert!(view.state().pending_reads.is_empty());
}

#[test]
fn coordinator_snapshot_resolves_unknown_read_ack_without_replay() {
    let (view, mut events) = fixture();
    view.state().pending_reads.insert(
        "unknown-read".into(),
        PendingRead {
            conversation: "root".into(),
            append_seq: 2,
            failed: true,
            confirmed: false,
        },
    );
    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot.reads.push(TeamBoardReadState {
        conversation_id: "root".into(),
        identity: TeamBoardIdentity::User,
        append_seq: 2,
    });
    view.state().update(snapshot);
    assert_eq!(
        view.state().take_confirmed_failed_reads(),
        vec!["unknown-read".to_string()]
    );
    assert!(view.state().pending_reads.is_empty());
    assert!(events.try_recv().is_err());
}

#[test]
fn rejected_mention_keeps_draft_and_correction_uses_a_new_operation() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("@Reviewer".into());
    key(&mut view, KeyCode::Tab);
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(original) = events.try_recv().unwrap() else {
        panic!("post missing")
    };
    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot
        .members
        .retain(|member| member.author.identity == TeamBoardIdentity::User);
    view.state().update(snapshot);
    assert!(!view.state().operation_rejected(
        &original.operation_id,
        "mentioned agent is no longer enrolled"
    ));
    assert!(view.state().pending.is_empty());
    assert_eq!(
        view.state().composer.as_ref().unwrap().current_text(),
        "@Reviewer "
    );
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .mention_bindings()
            .len(),
        1
    );
    assert!(
        view.state()
            .notice
            .as_deref()
            .unwrap()
            .contains("mentioned agent is no longer enrolled")
    );
    key(&mut view, KeyCode::Home);
    key(&mut view, KeyCode::Delete);
    view.handle_paste("Corrected post".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(corrected) = events.try_recv().unwrap() else {
        panic!("corrected post missing")
    };
    assert_ne!(original.operation_id, corrected.operation_id);
    assert!(
        matches!(corrected.request, TeamRequest::Board { request: TeamBoardParams::Post { mentions, body, .. } } if mentions.is_empty() && body.trim() == "Corrected post")
    );
}

#[test]
fn rejected_read_marker_waits_for_explicit_retry_instead_of_redraw() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Enter);
    render(&view, 70, 19);
    assert_eq!(view.state().take_frame_read(true), Some(("root".into(), 2)));
    view.state().pending_reads.insert(
        "rejected-read".into(),
        PendingRead {
            conversation: "root".into(),
            append_seq: 2,
            failed: false,
            confirmed: false,
        },
    );
    assert!(
        view.state()
            .operation_rejected("rejected-read", "cursor exceeds conversation")
    );
    render(&view, 70, 19);
    assert_eq!(view.state().take_frame_read(true), None);
    assert!(events.try_recv().is_err());
    view.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsTeamRetry(id) if id == "rejected-read")
    );
}

#[test]
fn post_topic_of_only_hashes_and_space_defaults_to_general() {
    let (mut view, mut events) = fixture();
    view.state().topic = " ##  ".into();
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("A new post".into());
    key(&mut view, KeyCode::Enter);
    let AppEvent::AgentsBoardOperation(operation) = events.try_recv().unwrap() else {
        panic!("post missing")
    };
    assert!(
        matches!(operation.request, TeamRequest::Board { request: TeamBoardParams::Post { topic, .. } } if topic == "general")
    );
}
