use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn synchronized_directory_replaces_removed_teammates_without_echo_or_regression() -> Result<()>
{
    let (_home, service, address, board) = fixture().await?;
    let changes = service.subscribe();
    let entry = TeamDirectoryEntry {
        address: address.clone(),
        alias: "Reviewer".into(),
        manually_held: false,
        continuation_turn_id: None,
        discovery: Default::default(),
        current_model: None,
        current_effort: None,
        next_model: None,
        next_effort: None,
    };
    let params = |revision, data| TeamSyncParams {
        administration: Vec::new(),
        retirements: Vec::new(),
        board_purges: Vec::new(),
        coordinator_id: "coordinator".into(),
        directory: TeamDirectorySnapshot { revision, data },
        board: Vec::new(),
        board_state: board.clone(),
        cancellation_adapters: Vec::new(),
        message_status: Default::default(),
    };
    service.sync(params(1, vec![entry.clone()])).await?;
    let first = service
        .agent_action(
            ThreadId::from_string(&address.thread_id)?,
            TeamAgentActionParams::Directory {
                query: None,
                details: true,
                cursor: None,
                limit: None,
            },
            None,
        )
        .await?;
    assert!(
        first.result["lastSynchronizedAt"]
            .as_i64()
            .is_some_and(|at| at > 0)
    );
    assert_eq!(
        first.result,
        serde_json::json!({"data":[entry], "nextCursor":null, "revision":1, "lastSynchronizedAt":first.result["lastSynchronizedAt"]})
    );
    service.sync(params(2, Vec::new())).await?;
    let removed = service
        .agent_action(
            ThreadId::from_string(&address.thread_id)?,
            TeamAgentActionParams::Directory {
                query: None,
                details: true,
                cursor: None,
                limit: None,
            },
            None,
        )
        .await?;
    assert!(removed.result["lastSynchronizedAt"].is_i64());
    assert_eq!(
        removed.result,
        serde_json::json!({"data":[], "nextCursor":null, "revision":2, "lastSynchronizedAt":removed.result["lastSynchronizedAt"]})
    );
    assert!(service.sync(params(1, vec![entry.clone()])).await.is_err());
    assert!(service.sync(params(2, vec![entry])).await.is_err());
    assert_eq!(
        service
            .agent_action(
                ThreadId::from_string(&address.thread_id)?,
                TeamAgentActionParams::Directory {
                    query: None,
                    details: true,
                    cursor: None,
                    limit: None
                },
                None
            )
            .await?
            .result,
        removed.result
    );
    assert!(!changes.has_changed()?);
    Ok(())
}
