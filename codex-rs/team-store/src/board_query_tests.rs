use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::query_board;

pub(super) fn entry(id: &str, conversation: &str, sequence: i64, body: &str) -> TeamBoardEntry {
    TeamBoardEntry {
        message: TeamMessage {
            id: id.into(),
            sender: TeamAddress {
                machine_id: "worker".into(),
                server_id: "primary".into(),
                thread_id: "agent".into(),
            },
            recipient: None,
            kind: TeamMessageKind::Board,
            body: body.into(),
            reply_to: (id != conversation).then(|| conversation.into()),
            topic: Some("review".into()),
            created_at: 100 - sequence,
        },
        conversation_id: conversation.into(),
        append_seq: sequence,
        author: TeamBoardAuthor {
            identity: TeamBoardIdentity::Agent {
                address: TeamAddress {
                    machine_id: "worker".into(),
                    server_id: "primary".into(),
                    thread_id: "agent".into(),
                },
            },
            name: "Reviewer".into(),
            machine_name: Some("Worker".into()),
            legacy: false,
        },
        mentions: Vec::new(),
        notifications: Vec::new(),
    }
}

#[test]
fn board_pages_use_coordinator_order_and_search_all_replies() -> Result<()> {
    let snapshot = TeamBoardSnapshot {
        revision: 7,
        entries: vec![
            entry("first", "first", 1, "Original"),
            entry("second", "second", 2, "New root"),
            entry("reply", "first", 3, "Needle in reply"),
        ],
        ..Default::default()
    };
    let query = TeamBoardParams::List {
        filter: TeamBoardFilter::All,
        query: None,
        topic: None,
        cursor: None,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::List {
        data, next_cursor, ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &query)?
    else {
        panic!("list response");
    };
    assert_eq!(
        data.iter()
            .map(|conversation| (
                conversation.id.as_str(),
                conversation.reply_count,
                conversation.unread
            ))
            .collect::<Vec<_>>(),
        vec![("first", 1, true)]
    );
    let next = TeamBoardParams::List {
        filter: TeamBoardFilter::All,
        query: None,
        topic: None,
        cursor: next_cursor,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::List {
        data, next_cursor, ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &next)?
    else {
        panic!("list response");
    };
    assert_eq!(
        data.iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );
    assert_eq!(next_cursor, None);
    let search = TeamBoardParams::List {
        filter: TeamBoardFilter::All,
        query: Some("needle".into()),
        topic: None,
        cursor: None,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::List { data, .. } =
        query_board(&snapshot, &TeamBoardIdentity::User, &search)?
    else {
        panic!("list response");
    };
    assert_eq!(
        data.iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first"]
    );
    let mut newer = snapshot;
    newer.revision += 1;
    // Read/subscription revisions do not change immutable discovery ordering.
    let TeamBoardResponse::List { data, .. } =
        query_board(&newer, &TeamBoardIdentity::User, &next)?
    else {
        panic!("list response");
    };
    assert_eq!(
        data.iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );
    Ok(())
}

#[test]
fn board_read_cursor_and_unread_are_independent_of_wall_time() -> Result<()> {
    let first = entry("root", "root", 1, "Root");
    let reply = entry("reply", "root", 2, "Reply");
    let snapshot = TeamBoardSnapshot {
        revision: 4,
        entries: vec![reply.clone(), first.clone()],
        reads: vec![TeamBoardReadState {
            conversation_id: "root".into(),
            identity: TeamBoardIdentity::User,
            append_seq: 1,
        }],
        ..Default::default()
    };
    let query = TeamBoardParams::Read {
        conversation_id: "root".into(),
        cursor: None,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::Read {
        data,
        conversation,
        next_cursor,
        ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &query)?
    else {
        panic!("read response");
    };
    assert_eq!(data, vec![first]);
    assert!(conversation.unread);
    assert_eq!(next_cursor, Some("1".into()));
    let next = TeamBoardParams::Read {
        conversation_id: "root".into(),
        cursor: next_cursor,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::Read {
        data, next_cursor, ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &next)?
    else {
        panic!("read response");
    };
    assert_eq!(data, vec![reply]);
    assert_eq!(next_cursor, None);
    Ok(())
}

#[test]
fn topic_previews_use_actual_authors_and_topic_following_includes_reply_unread() -> Result<()> {
    let first = entry("first", "first", 1, "Original");
    let mut reply = entry("reply", "first", 4, "Newest reply");
    reply.author.identity = TeamBoardIdentity::User;
    reply.author.name = "User".into();
    let mut second = entry("second", "second", 3, "Other topic");
    second.message.topic = Some("Review".into());
    let viewer = first.author.identity.clone();
    let snapshot = TeamBoardSnapshot {
        revision: 9,
        entries: vec![second, reply, first],
        topic_subscriptions: vec![
            TeamBoardTopicSubscription {
                mode: TeamBoardSubscriptionMode::Watch,
                topic: "review".into(),
                identity: viewer.clone(),
            },
            TeamBoardTopicSubscription {
                mode: TeamBoardSubscriptionMode::Wake,
                topic: "empty".into(),
                identity: viewer.clone(),
            },
        ],
        reads: vec![TeamBoardReadState {
            conversation_id: "first".into(),
            identity: viewer.clone(),
            append_seq: 1,
        }],
        ..Default::default()
    };
    let original = snapshot.clone();
    let topics = super::topic_overviews(&snapshot, &viewer);
    assert_eq!(
        topics
            .iter()
            .map(|topic| (
                topic.topic.as_str(),
                topic.conversation_count,
                topic.message_count,
                topic.unread_count,
                topic.subscription_mode
            ))
            .collect::<Vec<_>>(),
        vec![
            ("review", 1, 2, 1, TeamBoardSubscriptionMode::Watch),
            ("Review", 1, 1, 0, TeamBoardSubscriptionMode::Off),
            ("empty", 0, 0, 0, TeamBoardSubscriptionMode::Wake),
        ]
    );
    assert_eq!(
        topics[0]
            .participants
            .iter()
            .map(|author| author.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Reviewer", "User"]
    );
    assert!(topics[2].participants.is_empty());
    assert_eq!(topics[0].latest.as_ref().unwrap().message_id, "reply");
    let query = TeamBoardParams::List {
        filter: TeamBoardFilter::Following,
        query: None,
        topic: None,
        cursor: None,
        limit: None,
        identity: None,
    };
    let TeamBoardResponse::List { data, .. } = query_board(&snapshot, &viewer, &query)? else {
        panic!("list")
    };
    assert_eq!(
        data.iter()
            .map(|conversation| (
                conversation.id.as_str(),
                conversation.following,
                conversation.unread,
                conversation.subscription_mode,
                conversation.topic_subscription_mode
            ))
            .collect::<Vec<_>>(),
        vec![(
            "first",
            false,
            true,
            TeamBoardSubscriptionMode::Off,
            TeamBoardSubscriptionMode::Watch
        )]
    );
    assert_eq!(snapshot, original);
    Ok(())
}

#[test]
fn topic_overview_pages_are_revision_bound_and_ordered_by_append_sequence() -> Result<()> {
    let mut first = entry("first", "first", 1, "First");
    first.message.topic = Some("z".into());
    let mut second = entry("second", "second", 2, "Second");
    second.message.topic = Some("a".into());
    let mut snapshot = TeamBoardSnapshot {
        revision: 2,
        entries: vec![first, second],
        ..Default::default()
    };
    let query = TeamBoardParams::TopicOverview {
        cursor: None,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::TopicOverview {
        data, next_cursor, ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &query)?
    else {
        panic!("topics")
    };
    assert_eq!(
        data.iter()
            .map(|topic| topic.topic.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    let query = TeamBoardParams::TopicOverview {
        cursor: next_cursor,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::TopicOverview {
        data, next_cursor, ..
    } = query_board(&snapshot, &TeamBoardIdentity::User, &query)?
    else {
        panic!("topics")
    };
    assert_eq!(
        data.iter()
            .map(|topic| topic.topic.as_str())
            .collect::<Vec<_>>(),
        vec!["z"]
    );
    assert_eq!(next_cursor, None);
    snapshot.revision += 1;
    let TeamBoardResponse::TopicOverview { data, .. } =
        query_board(&snapshot, &TeamBoardIdentity::User, &query)?
    else {
        panic!("topic overview response");
    };
    assert_eq!(
        data.iter()
            .map(|topic| topic.topic.as_str())
            .collect::<Vec<_>>(),
        vec!["z"]
    );
    Ok(())
}

#[test]
fn topic_activity_previews_bound_unicode_excerpts_without_truncating_stored_posts() {
    let body = format!(" \n\n {}\nFull additional context", "🦀".repeat(300));
    let snapshot = TeamBoardSnapshot {
        entries: (1..=5)
            .map(|sequence| {
                entry(
                    &sequence.to_string(),
                    &sequence.to_string(),
                    sequence,
                    &body,
                )
            })
            .collect(),
        ..Default::default()
    };
    let topics = super::topic_overviews(&snapshot, &TeamBoardIdentity::User);
    let topic = &topics[0];
    assert_eq!(
        topic
            .recent_conversations
            .iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>(),
        vec!["5", "4", "3"]
    );
    assert_eq!(topic.conversation_count, 5);
    let expected = format!("{}…", "🦀".repeat(239));
    assert_eq!(topic.latest.as_ref().unwrap().excerpt, expected);
    assert!(
        topic
            .recent_conversations
            .iter()
            .all(|conversation| conversation.root.excerpt == expected
                && conversation.latest.excerpt == expected)
    );
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| entry.message.body == body)
    );
}
