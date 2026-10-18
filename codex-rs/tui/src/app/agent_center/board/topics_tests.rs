use super::*;
use pretty_assertions::assert_eq;

fn topic_target() -> TeamBoardSubscriptionTarget {
    TeamBoardSubscriptionTarget::Topic {
        topic: "release".into(),
    }
}

#[test]
fn topic_preview_scrolls_to_subscriptions_at_wide_and_narrow_sizes() {
    let (mut view, mut events) = fixture();
    for index in 3..30 {
        let id = format!("participant-{index}");
        let mut reply = entry(&id, "root", index, "Latest collaboration update", false);
        reply.author.name = id.clone();
        let TeamBoardIdentity::Agent { address } = &mut reply.author.identity else {
            unreachable!();
        };
        address.thread_id = id;
        view.state().snapshot.entries.push(reply);
    }
    view.state()
        .snapshot
        .topic_subscriptions
        .push(TeamBoardTopicSubscription {
            topic: "release".into(),
            identity: TeamBoardIdentity::User,
            mode: TeamBoardSubscriptionMode::Watch,
        });
    key(&mut view, KeyCode::Char('o'));
    key(&mut view, KeyCode::Tab);
    for (width, height) in [(120, 18), (45, 12)] {
        key(&mut view, KeyCode::Home);
        assert!(!render(&view, width, height).contains("Subscriptions"));
        key(&mut view, KeyCode::PageDown);
        assert!(view.state().topic_preview_scroll > 0);
        key(&mut view, KeyCode::End);
        let bottom = render(&view, width, height);
        assert!(bottom.contains("Subscriptions"));
        assert!(bottom.contains("You · Watch"));
        insta::assert_snapshot!(format!("board_topic_preview_bottom_{width}"), bottom);
        assert!(view.state().take_frame_read(true).is_none());
    }
    key(&mut view, KeyCode::Char('h'));
    assert!(view.state().topics_open);
    assert!(!view.state().topic_preview_focused);
    assert!(events.try_recv().is_err());
}

#[test]
fn topic_search_matches_recent_reply_excerpt() {
    let (view, _) = fixture();
    view.state().query = "asynchronous".into();
    assert_eq!(
        view.state()
            .topics()
            .iter()
            .map(|topic| topic.topic.as_str())
            .collect::<Vec<_>>(),
        ["release"]
    );
}

#[test]
fn topics_navigation_keeps_exact_topic_and_never_marks_previews_read() {
    let (mut view, mut events) = fixture();
    let mut other = entry("other", "other", 3, "Different case", false);
    other.message.topic = Some("Release".into());
    view.state().snapshot.entries.push(other);
    key(&mut view, KeyCode::Char('o'));
    assert!(view.state().topics_open);
    assert_eq!(view.state().selected_topic.as_deref(), Some("Release"));
    let frame = render(&view, 110, 24);
    assert!(frame.contains("#Release"));
    assert!(view.state().take_frame_read(true).is_none());
    assert!(events.try_recv().is_err());

    key(&mut view, KeyCode::Char('j'));
    assert_eq!(view.state().selected_topic.as_deref(), Some("release"));
    key(&mut view, KeyCode::Char('l'));
    assert_eq!(view.state().topic_filter.as_deref(), Some("release"));
    assert_eq!(
        view.state()
            .roots()
            .iter()
            .map(|entry| entry.message.id.as_str())
            .collect::<Vec<_>>(),
        ["root"]
    );
    key(&mut view, KeyCode::Char('l'));
    assert_eq!(view.state().opened.as_deref(), Some("root"));
    key(&mut view, KeyCode::Char('h'));
    assert!(view.state().opened.is_none());
    assert!(!view.state().topics_open);
    key(&mut view, KeyCode::Char('h'));
    assert!(view.state().topics_open);
    key(&mut view, KeyCode::Char('h'));
    assert!(!view.state().topics_open);
    assert!(view.state().topic_filter.is_none());
}

#[test]
fn topic_watch_includes_reply_unread_without_direct_follow() {
    let (mut view, _) = fixture();
    {
        let mut state = view.state();
        state.snapshot.followers.clear();
        state
            .snapshot
            .topic_subscriptions
            .push(TeamBoardTopicSubscription {
                topic: "release".into(),
                identity: TeamBoardIdentity::User,
                mode: TeamBoardSubscriptionMode::Watch,
            });
        state.snapshot.reads.push(TeamBoardReadState {
            conversation_id: "root".into(),
            identity: TeamBoardIdentity::User,
            append_seq: 1,
        });
    }
    key(&mut view, KeyCode::Tab);
    assert!(!view.state().following("root"));
    assert_eq!(view.state().roots().len(), 1);
    assert!(view.state().unread("root"));
    assert_eq!(view.state().topics()[0].unread_count, 1);
    assert!(render(&view, 120, 24).contains("Watching #release"));
}

#[test]
fn revisiting_topic_restores_feed_selection_and_scroll_anchor() {
    let (mut view, _) = fixture();
    view.state()
        .snapshot
        .entries
        .push(entry("other", "other", 3, "Second release post", false));
    key(&mut view, KeyCode::Char('o'));
    key(&mut view, KeyCode::Char('l'));
    key(&mut view, KeyCode::Char('j'));
    view.state().feed_scroll = 1;
    assert_eq!(view.state().selected.as_deref(), Some("root"));
    key(&mut view, KeyCode::Char('h'));
    key(&mut view, KeyCode::Char('l'));
    assert_eq!(view.state().selected.as_deref(), Some("root"));
    assert_eq!(view.state().feed_scroll, 1);
}

#[test]
fn subscription_confirmation_requires_acceptance_and_observed_revision() {
    let (view, _) = fixture();
    let mut state = view.state();
    let operation = state
        .begin_subscription(
            topic_target(),
            TeamBoardIdentity::User,
            TeamBoardSubscriptionMode::Watch,
        )
        .unwrap();
    let id = operation.operation_id;
    assert!(
        state
            .begin_subscription(
                topic_target(),
                TeamBoardIdentity::User,
                TeamBoardSubscriptionMode::Off
            )
            .is_none()
    );
    assert_eq!(
        state.subscription_mode(&topic_target(), &TeamBoardIdentity::User),
        TeamBoardSubscriptionMode::Off
    );

    let mut snapshot = state.snapshot.clone();
    snapshot.revision = 5;
    snapshot
        .topic_subscriptions
        .push(TeamBoardTopicSubscription {
            topic: "release".into(),
            identity: TeamBoardIdentity::User,
            mode: TeamBoardSubscriptionMode::Watch,
        });
    state.update(snapshot);
    assert!(state.pending_subscriptions.contains_key(&id));
    state.subscription_accepted(&id, 6);
    assert!(state.pending_subscriptions.contains_key(&id));
    let mut newer = state.snapshot.clone();
    newer.revision = 6;
    state.update(newer);
    assert!(state.pending_subscriptions.is_empty());
    assert_eq!(
        state.subscription_mode(&topic_target(), &TeamBoardIdentity::User),
        TeamBoardSubscriptionMode::Watch
    );
}

#[test]
fn unknown_subscription_retries_original_operation_and_rejection_releases_lock() {
    let (mut view, mut events) = fixture();
    let operation = view
        .state()
        .begin_subscription(
            topic_target(),
            TeamBoardIdentity::User,
            TeamBoardSubscriptionMode::Watch,
        )
        .unwrap();
    assert!(
        !view
            .state()
            .operation_finished(&operation.operation_id, false)
    );
    view.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::AgentsTeamRetry(id) if id == operation.operation_id)
    );
    assert!(
        view.state()
            .subscription_status(&topic_target(), &TeamBoardIdentity::User)
            .contains("unconfirmed")
    );
    view.state()
        .operation_rejected(&operation.operation_id, "not enrolled");
    assert!(view.state().pending_subscriptions.is_empty());
    assert_eq!(
        view.state()
            .subscription_mode(&topic_target(), &TeamBoardIdentity::User),
        TeamBoardSubscriptionMode::Off
    );
}

#[test]
fn topics_snapshots_and_draft_preservation() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Char('o'));
    let mut frames = Vec::new();
    for (width, height) in [(120, 24), (60, 16), (36, 7)] {
        frames.push(format!(
            "{width}x{height}\n{}",
            render(&view, width, height)
        ));
    }
    insta::assert_snapshot!("board_topics_sizes", frames.join("\n\n"));
    key(&mut view, KeyCode::Char('n'));
    view.handle_paste("Keep topic draft".into());
    key(&mut view, KeyCode::Esc);
    key(&mut view, KeyCode::Char('l'));
    key(&mut view, KeyCode::Char('n'));
    assert_eq!(
        view.state()
            .composer
            .as_ref()
            .unwrap()
            .recovery_snapshot()
            .text,
        "Keep topic draft"
    );
}

#[test]
fn subscription_pending_snapshot_keeps_confirmed_mode() {
    let (mut view, _) = fixture();
    key(&mut view, KeyCode::Char('o'));
    let operation = view
        .state()
        .begin_subscription(
            topic_target(),
            TeamBoardIdentity::User,
            TeamBoardSubscriptionMode::Watch,
        )
        .unwrap();
    let pending = render(&view, 100, 18);
    view.state()
        .operation_finished(&operation.operation_id, false);
    let unknown = render(&view, 100, 18);
    insta::assert_snapshot!(
        "board_subscription_pending",
        format!("Pending\n{pending}\n\nUnknown\n{unknown}")
    );
}
