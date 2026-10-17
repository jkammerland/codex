use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::query_board;

fn entry(id: &str, conversation: &str, sequence: i64, body: &str) -> TeamBoardEntry {
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
    assert!(query_board(&newer, &TeamBoardIdentity::User, &next).is_err());
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
