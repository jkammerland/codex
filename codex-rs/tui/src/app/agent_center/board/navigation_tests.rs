use super::*;
use pretty_assertions::assert_eq;

#[test]
fn feed_hjkl_and_arrows_preserve_selection_and_stay_on_board() {
    let (mut view, mut events) = fixture();
    for seq in 3..=9 {
        let id = format!("post-{seq}");
        view.state()
            .snapshot
            .entries
            .push(entry(&id, &id, seq, &format!("Post {seq}"), true));
    }
    view.state().selected = Some("post-9".into());
    insta::assert_snapshot!("board_feed_overflow_narrow", render(&view, 40, 18));

    key(&mut view, KeyCode::Char('j'));
    assert_eq!(view.state().selected.as_deref(), Some("post-8"));
    key(&mut view, KeyCode::Char('k'));
    assert_eq!(view.state().selected.as_deref(), Some("post-9"));
    key(&mut view, KeyCode::Down);
    assert_eq!(view.state().selected.as_deref(), Some("post-8"));
    key(&mut view, KeyCode::Up);
    assert_eq!(view.state().selected.as_deref(), Some("post-9"));

    for _ in 0..12 {
        key(&mut view, KeyCode::Char('j'));
        render(&view, 40, 18);
    }
    assert_eq!(view.state().selected.as_deref(), Some("root"));
    assert!(render(&view, 40, 18).contains("Posts 6-8/8"));
    let feed_scroll = view.state().feed_scroll;

    for (open, back) in [
        (KeyCode::Char('l'), KeyCode::Char('h')),
        (KeyCode::Right, KeyCode::Left),
        (KeyCode::Enter, KeyCode::Esc),
    ] {
        key(&mut view, open);
        assert_eq!(view.state().opened.as_deref(), Some("root"));
        key(&mut view, back);
        assert!(view.state().opened.is_none());
        assert_eq!(view.state().selected.as_deref(), Some("root"));
        assert_eq!(view.state().feed_scroll, feed_scroll);
        assert!(view.state().active);
    }
    assert!(events.try_recv().is_err());
    key(&mut view, KeyCode::Char('h'));
    assert!(!view.state().active);
    assert!(matches!(
        events.try_recv(),
        Ok(AppEvent::OpenAgentsOverview)
    ));
}

#[test]
fn navigation_letters_remain_literal_in_composer_search_and_topic() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Char('n'));
    for ch in "hjklvF".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .current_text_with_pending(),
        "hjklvF"
    );
    key(&mut view, KeyCode::Esc);

    key(&mut view, KeyCode::Char('/'));
    for ch in "hjkl".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    key(&mut view, KeyCode::Enter);
    assert_eq!(view.state().query, "hjkl");
    assert!(view.state().opened.is_none());
    view.state().notice = Some("Posted · notifications follow team Pause/Stop controls".into());
    let filtered = render(&view, 40, 12);
    assert!(filtered.contains("Search: hjkl · / edit"));
    assert!(filtered.contains("j/k ↑/↓ move · l/→/Enter open"));
    insta::assert_snapshot!("board_filtered_feed_narrow", filtered);

    key(&mut view, KeyCode::Char('#'));
    for ch in "hjkl".chars() {
        key(&mut view, KeyCode::Char(ch));
    }
    key(&mut view, KeyCode::Enter);
    assert_eq!(view.state().topic, "hjkl");
    assert!(!view.state().followers);
    assert!(events.try_recv().is_err());
}

#[test]
fn wrapped_post_navigation_shows_position_and_clamps_at_both_ends() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries.truncate(1);
    view.state().snapshot.entries[0].message.body = (0..120)
        .map(|index| format!("word{index:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    key(&mut view, KeyCode::Char('l'));
    let top = render(&view, 40, 12);
    assert!(top.contains("Lines 1-6/26 ↓"), "{top}");
    assert!(top.contains("word000 word001 word002 word003 word004"));
    key(&mut view, KeyCode::Char('k'));
    render(&view, 40, 12);
    assert_eq!(view.state().scroll, 0);
    key(&mut view, KeyCode::Char('j'));
    render(&view, 40, 12);
    assert_eq!(view.state().scroll, 1);
    key(&mut view, KeyCode::Up);
    render(&view, 40, 12);
    assert_eq!(view.state().scroll, 0);

    key(&mut view, KeyCode::PageDown);
    let page = render(&view, 40, 12);
    assert!(page.contains("Lines 7-12/26 ↑ ↓"), "{page}");
    assert!(page.lines().nth(4).unwrap().starts_with("word025"));
    for code in [KeyCode::Char('l'), KeyCode::Right, KeyCode::Enter] {
        key(&mut view, code);
        render(&view, 40, 12);
        assert_eq!(view.state().scroll, 6);
    }
    key(&mut view, KeyCode::PageUp);
    render(&view, 40, 12);
    assert_eq!(view.state().scroll, 0);
    key(&mut view, KeyCode::End);
    let bottom = render(&view, 40, 12);
    assert!(bottom.contains("Lines 21-26/26 ↑"), "{bottom}");
    assert!(bottom.contains("word115 word116 word117 word118 word119"));
    key(&mut view, KeyCode::Down);
    render(&view, 40, 12);
    assert_eq!(view.state().scroll, 20);
    assert!(view.state().at_bottom);
    key(&mut view, KeyCode::Home);
    assert_eq!(render(&view, 40, 12), top);
    insta::assert_snapshot!("board_wrapped_post_pages", [top, page, bottom].join("\n\n"));
}

#[test]
fn many_replies_require_contiguous_pages_and_keep_new_reply_position() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries.truncate(1);
    view.state().snapshot.entries[0].message.body = "Post".into();
    for seq in 2..=31 {
        view.state().snapshot.entries.push(entry(
            &format!("reply-{seq}"),
            "root",
            seq,
            &format!("Reply {seq:02}"),
            false,
        ));
    }
    key(&mut view, KeyCode::Char('l'));
    let top = render(&view, 40, 12);
    assert_eq!(view.state().displayed.as_ref().unwrap().end, 6);
    assert_eq!(top.lines().nth(9), Some("Reply 02"));
    assert!(!top.contains("Reply 03"), "{top}");
    assert_eq!(view.state().take_frame_read(true), Some(("root".into(), 2)));
    key(&mut view, KeyCode::End);
    let end = render(&view, 40, 12);
    assert!(end.contains("Reply 31"));
    assert_eq!(view.state().take_frame_read(true), None);

    // Returning to the top and paging through every row closes the unread gap.
    key(&mut view, KeyCode::Home);
    let mut replies = Vec::new();
    loop {
        let frame = render(&view, 40, 12);
        replies.extend(
            frame
                .lines()
                .filter_map(|line| line.strip_prefix("Reply "))
                .map(str::to_owned),
        );
        view.state().take_frame_read(true);
        if view.state().at_bottom {
            break;
        }
        key(&mut view, KeyCode::PageDown);
    }
    replies.dedup();
    assert_eq!(
        replies,
        (2..=31).map(|seq| format!("{seq:02}")).collect::<Vec<_>>()
    );
    assert_eq!(view.state().read_requested.get("root"), Some(&31));

    key(&mut view, KeyCode::PageUp);
    let before = render(&view, 40, 12);
    let scroll = view.state().scroll;
    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot
        .entries
        .push(entry("reply-32", "root", 32, "Reply 32", false));
    view.state().update(snapshot);
    let after = render(&view, 40, 12);
    assert_eq!(view.state().scroll, scroll);
    assert_eq!(
        before.lines().skip(4).take(6).collect::<Vec<_>>(),
        after.lines().skip(4).take(6).collect::<Vec<_>>()
    );
    assert!(after.contains("· 1 new"), "{after}");
    key(&mut view, KeyCode::End);
    render(&view, 40, 12);
    assert_eq!(
        view.state().take_frame_read(true),
        Some(("root".into(), 32))
    );

    let mut snapshot = view.state().snapshot.clone();
    snapshot.revision += 1;
    snapshot
        .entries
        .push(entry("reply-33", "root", 33, "Reply 33", false));
    view.state().update(snapshot);
    let latest = render(&view, 40, 12);
    assert!(latest.contains("Reply 33"));
    assert!(view.state().at_bottom);
    assert_eq!(
        view.state().take_frame_read(true),
        Some(("root".into(), 33))
    );
    insta::assert_snapshot!("board_new_reply_scrolled_up", after);
    insta::assert_snapshot!("board_many_replies_bottom_wide", render(&view, 120, 12));
}

#[test]
fn resizing_wrapped_post_retains_source_text_and_does_not_credit_unseen_rows() {
    let (mut view, _) = fixture();
    view.state().snapshot.entries.truncate(1);
    view.state().snapshot.entries[0].message.body = (0..200)
        .map(|index| format!("word{index:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    key(&mut view, KeyCode::Char('l'));
    render(&view, 40, 12);
    view.state().take_frame_read(true);
    key(&mut view, KeyCode::PageDown);
    let narrow = render(&view, 40, 12);
    assert!(narrow.lines().nth(4).unwrap().starts_with("word025"));
    view.state().take_frame_read(true);

    let wider = render(&view, 60, 12);
    assert!(wider.lines().nth(4).unwrap().contains("word025"), "{wider}");
    assert_eq!(view.state().take_frame_read(true), None);
    let wide = render(&view, 120, 12);
    assert!(wide.lines().nth(4).unwrap().contains("word025"), "{wide}");
    assert_eq!(view.state().take_frame_read(true), None);
    let taller = render(&view, 120, 18);
    assert_eq!(wide.lines().nth(4), taller.lines().nth(4));
    assert_eq!(view.state().take_frame_read(true), None);

    // A wider row starts before the anchor; that must not replace the original source byte.
    for width in [40, 60, 120, 40, 60, 120, 40] {
        let frame = render(&view, width, 12);
        let top = frame.lines().nth(4).unwrap();
        assert!(top.contains("word025"), "{frame}");
        if width == 40 {
            assert_eq!(Some(top), narrow.lines().nth(4));
        }
    }

    key(&mut view, KeyCode::End);
    render(&view, 120, 18);
    let bottom = render(&view, 40, 12);
    assert!(bottom.contains("word199"));
    assert!(view.state().at_bottom);
    assert_eq!(view.state().take_frame_read(true), None);
}

#[test]
fn board_cancel_preserves_drafts_and_back_visits_one_level() {
    for back in [KeyCode::Esc, KeyCode::Char('h'), KeyCode::Left] {
        let (mut view, mut events) = fixture();
        key(&mut view, KeyCode::Enter);
        key(&mut view, KeyCode::Char('r'));
        view.handle_paste("Keep this reply".into());
        view.on_ctrl_c();
        assert!(view.state().composing.is_none());
        assert_eq!(view.state().opened.as_deref(), Some("root"));
        key(&mut view, KeyCode::Char('r'));
        assert_eq!(
            view.state()
                .composer
                .as_ref()
                .unwrap()
                .current_text_with_pending(),
            "Keep this reply"
        );
        key(&mut view, KeyCode::Esc);

        key(&mut view, KeyCode::Char('/'));
        view.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!view.state().searching);
        assert_eq!(view.state().opened.as_deref(), Some("root"));
        view.on_ctrl_c();
        assert_eq!(view.state().opened.as_deref(), Some("root"));
        assert!(events.try_recv().is_err());

        key(&mut view, back);
        assert!(view.state().opened.is_none());
        assert!(events.try_recv().is_err());
        key(&mut view, back);
        assert!(matches!(
            events.try_recv(),
            Ok(AppEvent::OpenAgentsOverview)
        ));
    }
}

#[test]
fn board_tab_focus_does_not_filter_or_scroll_another_region() {
    let (mut view, mut events) = fixture();
    key(&mut view, KeyCode::Enter);
    render(&view, 120, 24);
    key(&mut view, KeyCode::Tab);
    assert!(view.state().filters_focused);
    assert!(view.state().filter == Filter::All);
    key(&mut view, KeyCode::Tab);
    assert!(!view.state().conversation_focused);
    assert!(!view.state().filters_focused);
    key(&mut view, KeyCode::Down);
    assert_eq!(view.state().scroll, 0);
    key(&mut view, KeyCode::BackTab);
    assert!(view.state().filters_focused);
    key(&mut view, KeyCode::Down);
    assert!(view.state().filter == Filter::Following);
    view.on_ctrl_c();
    assert!(view.state().active);
    assert!(events.try_recv().is_err());
    view.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(matches!(events.try_recv(), Ok(AppEvent::Exit(_))));
}

#[test]
fn feed_actions_target_its_selection_while_another_conversation_stays_open() {
    for action in ['r', 'f', 'F', 'd'] {
        let (mut view, mut events) = fixture();
        view.state()
            .snapshot
            .entries
            .push(entry("other", "other", 3, "Other conversation", true));
        key(&mut view, KeyCode::Enter);
        render(&view, 120, 24);
        key(&mut view, KeyCode::Tab);
        key(&mut view, KeyCode::Tab);
        key(&mut view, KeyCode::Up);
        assert_eq!(view.state().selected.as_deref(), Some("other"));
        assert_eq!(view.state().opened.as_deref(), Some("root"));
        key(&mut view, KeyCode::Char(action));
        match action {
            'r' => {
                assert_eq!(view.state().composing.as_deref(), Some("other"));
                assert_eq!(view.state().opened.as_deref(), Some("other"));
            }
            'f' => assert!(
                matches!(events.try_recv(), Ok(AppEvent::AgentsTeamRequest(TeamRequest::Board {
                request: TeamBoardParams::SubscriptionSet {
                    target: TeamBoardSubscriptionTarget::Conversation { conversation_id }, ..
                }
            })) if conversation_id == "other")
            ),
            'F' => assert!(
                matches!(events.try_recv(), Ok(AppEvent::AgentsBoardManageFollowers(id)) if id == "other")
            ),
            'd' => assert!(
                matches!(events.try_recv(), Ok(AppEvent::AgentsTeamMessage(id)) if id == "other")
            ),
            _ => unreachable!(),
        }
    }
}
