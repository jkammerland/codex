use super::super::tests::entry;
use super::*;
use pretty_assertions::assert_eq;

fn snapshot() -> TeamBoardSnapshot {
    TeamBoardSnapshot {
        revision: 7,
        entries: vec![
            entry("root", "root", 5, "Review the implementation"),
            entry("middle", "root", 12, "Investigate the async path"),
            entry("last", "root", 40, "The async path passed"),
        ],
        reads: vec![TeamBoardReadState {
            conversation_id: "root".into(),
            identity: TeamBoardIdentity::User,
            append_seq: 5,
        }],
        ..Default::default()
    }
}

fn request(
    mode: TeamBoardReadMode,
    anchor: Option<&str>,
    cursor: Option<String>,
) -> TeamBoardParams {
    TeamBoardParams::ReadPage {
        conversation_id: "root".into(),
        mode,
        anchor_message_id: anchor.map(str::to_owned),
        cursor,
        limit: Some(1),
        identity: None,
    }
}

fn page(snapshot: &TeamBoardSnapshot, request: TeamBoardParams) -> Result<TeamBoardReadPage> {
    let TeamBoardResponse::ReadPage { page } =
        query_agent_board(snapshot, &TeamBoardIdentity::User, &request)?
    else {
        panic!("expected read page");
    };
    Ok(*page)
}

#[test]
fn unread_pages_acknowledge_contiguously_across_sparse_global_sequences() -> Result<()> {
    let mut snapshot = snapshot();
    let original = snapshot.clone();
    let first = page(&snapshot, request(TeamBoardReadMode::Unread, None, None))?;
    assert_eq!(
        first
            .data
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["middle"]
    );
    assert_eq!(
        first.root.as_ref().map(|root| root.message_id.as_str()),
        Some("root")
    );
    assert_eq!(first.unread_count, 2);
    let second = page(
        &snapshot,
        request(TeamBoardReadMode::Unread, None, first.next_cursor.clone()),
    )?;
    assert_eq!(
        second
            .data
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["last"]
    );
    assert_eq!(second.root, None);
    assert_eq!(second.next_cursor, None);
    assert_eq!(snapshot, original);

    let last_token = second
        .acknowledgement_token
        .as_deref()
        .expect("second page token");
    assert!(
        validate_board_acknowledgement(&snapshot, &TeamBoardIdentity::User, last_token)
            .unwrap_err()
            .to_string()
            .contains("unread gap")
    );
    let first_token = first
        .acknowledgement_token
        .as_deref()
        .expect("first page token");
    let read = validate_board_acknowledgement(&snapshot, &TeamBoardIdentity::User, first_token)?;
    assert_eq!(read.append_seq, 12);
    snapshot.reads = vec![read];
    let read = validate_board_acknowledgement(&snapshot, &TeamBoardIdentity::User, last_token)?;
    assert_eq!(read.append_seq, 40);
    snapshot.reads = vec![read.clone()];
    assert_eq!(
        validate_board_acknowledgement(&snapshot, &TeamBoardIdentity::User, last_token)?,
        read
    );
    assert!(
        validate_board_acknowledgement(&snapshot, &snapshot.entries[0].author.identity, last_token)
            .is_err()
    );
    let empty = page(&snapshot, request(TeamBoardReadMode::Unread, None, None))?;
    assert!(empty.data.is_empty());
    assert_eq!(empty.acknowledgement_token, None);
    assert_eq!(empty.unread_count, 0);
    Ok(())
}

#[test]
fn history_and_around_reads_are_explicit_and_do_not_skip_unread_gaps() -> Result<()> {
    let snapshot = snapshot();
    let original = snapshot.clone();
    let history = page(&snapshot, request(TeamBoardReadMode::History, None, None))?;
    assert_eq!(history.data[0].id, "root");
    assert_eq!(history.root, None);
    let continued = page(
        &snapshot,
        request(TeamBoardReadMode::Unread, None, history.next_cursor),
    )?;
    assert_eq!(continued.data[0].id, "middle");
    let around = page(
        &snapshot,
        request(TeamBoardReadMode::Around, Some("last"), None),
    )?;
    assert_eq!(around.data[0].id, "last");
    assert!(
        validate_board_acknowledgement(
            &snapshot,
            &TeamBoardIdentity::User,
            around
                .acknowledgement_token
                .as_deref()
                .expect("around token")
        )
        .is_err()
    );
    assert!(page(&snapshot, request(TeamBoardReadMode::Around, None, None)).is_err());
    assert!(
        page(
            &snapshot,
            request(TeamBoardReadMode::Unread, Some("last"), None)
        )
        .is_err()
    );
    let around_middle = page(
        &snapshot,
        request(TeamBoardReadMode::Around, Some("middle"), None),
    )?;
    let continued = page(
        &snapshot,
        request(TeamBoardReadMode::Unread, None, around_middle.next_cursor),
    )?;
    assert_eq!(continued.data[0].id, "last");
    assert_eq!(snapshot, original);
    Ok(())
}

fn list(filter: TeamBoardFilter, cursor: Option<String>) -> TeamBoardParams {
    TeamBoardParams::List {
        filter,
        query: None,
        topic: None,
        cursor,
        limit: Some(1),
        identity: None,
    }
}

fn threads(
    snapshot: &TeamBoardSnapshot,
    request: TeamBoardParams,
) -> Result<(Vec<TeamBoardThreadSummary>, Option<String>)> {
    let TeamBoardResponse::Threads {
        data, next_cursor, ..
    } = query_agent_board(snapshot, &TeamBoardIdentity::User, &request)?
    else {
        panic!("expected compact threads");
    };
    Ok((data, next_cursor))
}

#[test]
fn discovery_pages_keep_their_order_during_posts_reads_and_unrelated_follows() -> Result<()> {
    let mut snapshot = TeamBoardSnapshot {
        revision: 3,
        entries: vec![
            entry("a", "a", 1, "A"),
            entry("b", "b", 2, "B"),
            entry("c", "c", 3, "C"),
        ],
        ..Default::default()
    };
    let (first, cursor) = threads(&snapshot, list(TeamBoardFilter::All, None))?;
    assert_eq!(first[0].id, "c");
    let second_request = list(TeamBoardFilter::All, cursor);
    snapshot.entries.push(entry("new-a", "a", 4, "New A reply"));
    snapshot.revision += 1;
    snapshot.reads.push(TeamBoardReadState {
        conversation_id: "c".into(),
        identity: TeamBoardIdentity::User,
        append_seq: 3,
    });
    snapshot
        .topic_subscriptions
        .push(TeamBoardTopicSubscription {
            topic: "review".into(),
            identity: snapshot.entries[0].author.identity.clone(),
            mode: TeamBoardSubscriptionMode::Wake,
        });
    let (second, cursor) = threads(&snapshot, second_request.clone())?;
    assert_eq!(second[0].id, "b");
    let (third, cursor) = threads(&snapshot, list(TeamBoardFilter::All, cursor))?;
    assert_eq!(third[0].id, "a");
    assert_eq!(third[0].latest.message_id, "a");
    assert_eq!(cursor, None);
    assert_eq!(
        threads(&snapshot, list(TeamBoardFilter::All, None))?.0[0]
            .latest
            .message_id,
        "new-a"
    );

    let mut purged = snapshot.clone();
    purged.entries.retain(|entry| entry.conversation_id != "b");
    purged.revision += 1;
    assert!(
        threads(&purged, second_request.clone())
            .unwrap_err()
            .to_string()
            .contains("removed")
    );
    let mut stale = snapshot;
    stale.revision = 1;
    assert!(
        threads(&stale, second_request)
            .unwrap_err()
            .to_string()
            .contains("not synchronized")
    );
    Ok(())
}

#[test]
fn unread_discovery_uses_current_markers_without_invalidating_the_content_page() -> Result<()> {
    let mut snapshot = TeamBoardSnapshot {
        revision: 3,
        entries: vec![
            entry("a", "a", 1, "A"),
            entry("b", "b", 2, "B"),
            entry("c", "c", 3, "C"),
        ],
        ..Default::default()
    };
    let (_, cursor) = threads(&snapshot, list(TeamBoardFilter::Unread, None))?;
    snapshot.reads.push(TeamBoardReadState {
        conversation_id: "b".into(),
        identity: TeamBoardIdentity::User,
        append_seq: 2,
    });
    snapshot.revision += 1;
    let (data, cursor) = threads(&snapshot, list(TeamBoardFilter::Unread, cursor))?;
    assert_eq!(
        data.iter()
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(cursor, None);
    Ok(())
}

#[test]
fn following_pages_bind_membership_but_not_watch_wake_delivery_modes() -> Result<()> {
    let mut snapshot = TeamBoardSnapshot {
        revision: 3,
        entries: vec![entry("a", "a", 1, "A"), entry("b", "b", 2, "B")],
        topic_subscriptions: vec![TeamBoardTopicSubscription {
            topic: "review".into(),
            identity: TeamBoardIdentity::User,
            mode: TeamBoardSubscriptionMode::Watch,
        }],
        ..Default::default()
    };
    let (_, cursor) = threads(&snapshot, list(TeamBoardFilter::Following, None))?;
    let next = list(TeamBoardFilter::Following, cursor);
    snapshot.topic_subscriptions[0].mode = TeamBoardSubscriptionMode::Wake;
    snapshot.revision += 1;
    assert_eq!(threads(&snapshot, next.clone())?.0[0].id, "a");
    snapshot.topic_subscriptions.clear();
    snapshot.revision += 1;
    assert!(
        threads(&snapshot, next)
            .unwrap_err()
            .to_string()
            .contains("membership changed")
    );
    Ok(())
}

#[test]
fn compact_search_returns_navigation_hits_without_full_repeated_bodies() -> Result<()> {
    let mut snapshot = snapshot();
    snapshot.entries[0].message.body = format!("Root summary\n{}", "root payload ".repeat(100));
    snapshot.entries[1].message.body = format!("First line\n{}Needle in middle", "İ🧪".repeat(300));
    snapshot.entries[2].message.body = "Otherword in last".into();
    let query = TeamBoardParams::List {
        filter: TeamBoardFilter::All,
        query: Some("needle otherword #REVIEW".into()),
        topic: None,
        cursor: None,
        limit: None,
        identity: None,
    };
    let (data, _) = threads(&snapshot, query)?;
    assert_eq!(data.len(), 1);
    assert_eq!(
        data[0]
            .matches
            .iter()
            .map(|entry| entry.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["middle", "last"]
    );
    let serialized = serde_json::to_string(&data)?;
    assert!(!serialized.contains("root payload"));
    assert_eq!(data[0].title, "Root summary");
    assert_eq!(data[0].unread_count, 2);
    assert!(data[0].matches[0].excerpt.contains("Needle"));
    assert!(data[0].matches[0].excerpt.chars().count() <= 242);
    Ok(())
}

#[test]
fn agent_page_size_defaults_to_twenty_without_changing_legacy_operator_reads() -> Result<()> {
    let snapshot = TeamBoardSnapshot {
        revision: 21,
        entries: (1..=21)
            .map(|sequence| entry(&format!("{sequence}"), "1", sequence, "entry"))
            .collect(),
        ..Default::default()
    };
    let TeamBoardResponse::ReadPage { page } = query_agent_board(
        &snapshot,
        &TeamBoardIdentity::User,
        &TeamBoardParams::ReadPage {
            conversation_id: "1".into(),
            mode: TeamBoardReadMode::Unread,
            anchor_message_id: None,
            cursor: None,
            limit: None,
            identity: None,
        },
    )?
    else {
        panic!("read page");
    };
    assert_eq!(page.data.len(), 20);
    assert!(page.next_cursor.is_some());
    let TeamBoardResponse::Read {
        data, next_cursor, ..
    } = super::super::query_board(
        &snapshot,
        &TeamBoardIdentity::User,
        &TeamBoardParams::Read {
            conversation_id: "1".into(),
            cursor: None,
            limit: None,
            identity: None,
        },
    )?
    else {
        panic!("legacy read");
    };
    assert_eq!(data.len(), 21);
    assert_eq!(next_cursor, None);
    Ok(())
}
