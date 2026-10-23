use super::*;
use pretty_assertions::assert_eq;

async fn read(
    coordinator: &Coordinator,
    root: &str,
    cursor: Option<String>,
) -> Result<TeamBoardReadPage> {
    let TeamResponse::Board {
        response: TeamBoardResponse::ReadPage { page },
    } = coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::ReadPage {
                conversation_id: root.into(),
                mode: TeamBoardReadMode::Unread,
                anchor_message_id: None,
                cursor,
                limit: Some(1),
                identity: None,
            },
        })
        .await?
    else {
        panic!("read page")
    };
    Ok(*page)
}

#[tokio::test]
async fn acknowledgement_is_contiguous_durable_and_idempotent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = manifest(directory.path());
    let coordinator = Coordinator::open(config.clone()).await?;
    let root = post(&coordinator, "Root").await?;
    post(&coordinator, "Unrelated root").await?;
    coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::Reply {
                conversation_id: root.clone(),
                reply_to: None,
                body: "Reply".into(),
                mentions: Vec::new(),
            },
        })
        .await?;
    let first = read(&coordinator, &root, None).await?;
    let next = read(&coordinator, &root, first.next_cursor.clone()).await?;
    assert_eq!(first.data[0].append_seq, 1);
    assert_eq!(next.data[0].append_seq, 3);
    assert!(coordinator.board_snapshot().await?.reads.is_empty());

    let second_ack = TeamRequest::Board {
        request: TeamBoardParams::Acknowledge {
            token: next.acknowledgement_token.clone().expect("second token"),
        },
    };
    let rejected = TeamOperation::new(second_ack.clone());
    assert!(
        coordinator
            .execute_operation(rejected.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("unread gap")
    );
    assert!(coordinator.board_snapshot().await?.reads.is_empty());
    coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::Acknowledge {
                token: first.acknowledgement_token.expect("first token"),
            },
        })
        .await?;
    // A definitive rejection keeps its original result; a new attempt has a new operation identity.
    assert!(coordinator.execute_operation(rejected).await.is_err());
    let accepted = TeamOperation::new(second_ack.clone());
    let result = serde_json::to_value(coordinator.execute_operation(accepted.clone()).await?)?;
    let snapshot = coordinator.board_snapshot().await?;
    assert_eq!(
        snapshot.reads,
        vec![TeamBoardReadState {
            conversation_id: root.clone(),
            identity: TeamBoardIdentity::User,
            append_seq: 3,
        }]
    );
    assert_eq!(
        serde_json::to_value(coordinator.execute_operation(accepted.clone()).await?)?,
        result
    );
    coordinator.execute(second_ack).await?;
    assert_eq!(
        coordinator.board_snapshot().await?.revision,
        snapshot.revision
    );
    drop(coordinator);

    let reopened = Coordinator::open(config).await?;
    assert_eq!(reopened.board_snapshot().await?.reads, snapshot.reads);
    assert_eq!(
        serde_json::to_value(reopened.execute_operation(accepted).await?)?,
        result
    );
    assert!(read(&reopened, &root, None).await?.data.is_empty());
    Ok(())
}

#[tokio::test]
async fn agent_acknowledgement_uses_its_bound_identity_and_current_coordinator_marker() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let address = agent(&coordinator, "Reader").await?;
    let root = post(&coordinator, "Root").await?;
    coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::Reply {
                conversation_id: root.clone(),
                reply_to: None,
                body: "Reply".into(),
                mentions: Vec::new(),
            },
        })
        .await?;
    let snapshot = coordinator.board_snapshot().await?;
    let viewer = TeamBoardIdentity::Agent {
        address: address.clone(),
    };
    let request = |cursor| TeamBoardParams::ReadPage {
        conversation_id: root.clone(),
        mode: TeamBoardReadMode::Unread,
        anchor_message_id: None,
        cursor,
        limit: Some(1),
        identity: None,
    };
    let TeamBoardResponse::ReadPage { page: first } =
        codex_team_store::query_agent_board(&snapshot, &viewer, &request(None))?
    else {
        panic!("first page")
    };
    let TeamBoardResponse::ReadPage { page: next } =
        codex_team_store::query_agent_board(&snapshot, &viewer, &request(first.next_cursor))?
    else {
        panic!("next page")
    };
    // Both pages came from the same unacknowledged cache. Coordinator ordering,
    // not that stale cache, decides whether the second acknowledgement is valid.
    for token in [first.acknowledgement_token, next.acknowledgement_token] {
        agent_board(
            &coordinator,
            &address,
            TeamBoardParams::Acknowledge {
                token: token.expect("page token"),
            },
        )
        .await?;
    }
    assert_eq!(
        coordinator.board_snapshot().await?.reads,
        vec![TeamBoardReadState {
            conversation_id: root,
            identity: viewer,
            append_seq: 2,
        }]
    );
    Ok(())
}
