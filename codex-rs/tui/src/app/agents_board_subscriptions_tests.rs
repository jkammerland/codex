use super::*;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamBoardAuthor;
use codex_app_server_protocol::TeamBoardMember;
use codex_app_server_protocol::TeamBoardTopicSubscription;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn subscription_pickers_show_named_modes_and_preserve_existing_wake() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let identity = TeamBoardIdentity::Agent {
        address: TeamAddress {
            machine_id: "builder".into(),
            server_id: "main".into(),
            thread_id: "reviewer".into(),
        },
    };
    let target = TeamBoardSubscriptionTarget::Topic {
        topic: "release".into(),
    };
    {
        let mut state = app.agents_hosts.board.lock().unwrap();
        state.snapshot.members.push(TeamBoardMember {
            author: TeamBoardAuthor {
                identity: identity.clone(),
                name: "Reviewer".into(),
                machine_name: Some("Build machine".into()),
                legacy: false,
            },
            available: true,
        });
        state
            .snapshot
            .topic_subscriptions
            .push(TeamBoardTopicSubscription {
                topic: "release".into(),
                identity: identity.clone(),
                mode: TeamBoardSubscriptionMode::Wake,
            });
    }
    app.open_agents_board_subscriptions(target.clone());
    let members = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 90);
    app.open_agents_board_subscription_mode(target.clone(), identity.clone(), "Reviewer".into());
    let modes = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 90);
    insta::assert_snapshot!(
        "board_subscription_pickers",
        format!("Members\n{members}\n\nModes\n{modes}")
    );
    let state = app.agents_hosts.board.lock().unwrap();
    assert_eq!(
        state.subscription_mode(&target, &identity),
        TeamBoardSubscriptionMode::Wake
    );
    assert!(state.pending_subscriptions.is_empty());
}

#[tokio::test]
async fn pending_subscription_picker_disables_conflicting_mode_changes() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let target = TeamBoardSubscriptionTarget::Conversation {
        conversation_id: "review".into(),
    };
    app.agents_hosts
        .board
        .lock()
        .unwrap()
        .begin_subscription(
            target.clone(),
            TeamBoardIdentity::User,
            TeamBoardSubscriptionMode::Watch,
        )
        .unwrap();
    app.open_agents_board_subscription_mode(target, TeamBoardIdentity::User, "You".into());
    insta::assert_snapshot!(
        "board_subscription_picker_pending",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 80)
    );
}

#[test]
fn subscription_members_include_departed_names_and_exact_unknown_addresses_once() {
    use codex_app_server_protocol::TeamBoardFollower;

    let address = |thread: &str| TeamBoardIdentity::Agent {
        address: TeamAddress {
            machine_id: "worker".into(),
            server_id: "main".into(),
            thread_id: thread.into(),
        },
    };
    let current = address("current");
    let departed = address("departed");
    let unknown = address("unknown");
    let author = |identity: TeamBoardIdentity, name: &str| TeamBoardAuthor {
        identity,
        name: name.into(),
        machine_name: Some("Build machine".into()),
        legacy: false,
    };
    let snapshot = TeamBoardSnapshot {
        members: vec![TeamBoardMember {
            author: author(current.clone(), "Current"),
            available: false,
        }],
        followers: vec![
            TeamBoardFollower {
                conversation_id: "root".into(),
                identity: current.clone(),
                author: author(current.clone(), "Old current name"),
                mode: TeamBoardSubscriptionMode::Watch,
            },
            TeamBoardFollower {
                conversation_id: "root".into(),
                identity: departed.clone(),
                author: author(departed.clone(), "Last known reviewer"),
                mode: TeamBoardSubscriptionMode::Wake,
            },
        ],
        topic_subscriptions: vec![
            TeamBoardTopicSubscription {
                topic: "release".into(),
                identity: departed.clone(),
                mode: TeamBoardSubscriptionMode::Watch,
            },
            TeamBoardTopicSubscription {
                topic: "release".into(),
                identity: unknown.clone(),
                mode: TeamBoardSubscriptionMode::Wake,
            },
        ],
        ..Default::default()
    };
    let rows = subscription_participants(
        &snapshot,
        &TeamBoardSubscriptionTarget::Conversation {
            conversation_id: "root".into(),
        },
    );
    assert_eq!(
        rows.iter()
            .map(|row| (
                &row.author.identity,
                row.author.name.as_str(),
                row.enrolled,
                row.connected
            ))
            .collect::<Vec<_>>(),
        vec![
            (&TeamBoardIdentity::User, "User", true, true),
            (&current, "Current", true, false),
            (&departed, "Last known reviewer", false, false),
        ]
    );
    let rows = subscription_participants(
        &snapshot,
        &TeamBoardSubscriptionTarget::Topic {
            topic: "release".into(),
        },
    );
    assert_eq!(
        rows.iter()
            .map(|row| (&row.author.identity, row.author.name.as_str(), row.enrolled))
            .collect::<Vec<_>>(),
        vec![
            (&TeamBoardIdentity::User, "User", true),
            (&current, "Current", true),
            (&departed, "Last known reviewer", false),
            (&unknown, "worker/main/unknown", false),
        ]
    );
}

#[tokio::test]
async fn departed_subscription_picker_disables_wake_and_watch_but_emits_off() {
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    let identity = TeamBoardIdentity::Agent {
        address: TeamAddress {
            machine_id: "worker".into(),
            server_id: "main".into(),
            thread_id: "departed".into(),
        },
    };
    let target = TeamBoardSubscriptionTarget::Topic {
        topic: "release".into(),
    };
    app.agents_hosts
        .board
        .lock()
        .unwrap()
        .snapshot
        .topic_subscriptions
        .push(TeamBoardTopicSubscription {
            topic: "release".into(),
            identity: identity.clone(),
            mode: TeamBoardSubscriptionMode::Wake,
        });
    app.open_agents_board_subscriptions(target.clone());
    let members = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 100);
    app.open_agents_board_subscription_mode(
        target.clone(),
        identity.clone(),
        "worker/main/departed".into(),
    );
    let modes = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 100);
    insta::assert_snapshot!(
        "board_departed_subscription_picker",
        format!("Members\n{members}\n\nModes\n{modes}")
    );
    while events.try_recv().is_ok() {}
    app.chat_widget
        .handle_key_event(crossterm::event::KeyCode::End.into());
    app.chat_widget
        .handle_key_event(crossterm::event::KeyCode::Enter.into());
    let request = events
        .try_recv()
        .expect("Off remains selectable for a departed subscriber");
    let AppEvent::AgentsTeamRequest(codex_team::TeamRequest::Board { request }) = request else {
        panic!("subscription mutation")
    };
    assert_eq!(
        request,
        TeamBoardParams::SubscriptionSet {
            target,
            identity: Some(identity),
            mode: TeamBoardSubscriptionMode::Off
        }
    );
    assert!(events.try_recv().is_err());
}
