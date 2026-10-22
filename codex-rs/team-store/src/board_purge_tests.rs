use anyhow::Result;
use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use crate::BoardPurgeCommit;
use crate::TeamStore;

async fn fixture() -> Result<(tempfile::TempDir, TeamStore)> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let store = TeamStore::open(&sqlite, &home.path().join("team.sqlite")).await?;
    Ok((home, store))
}

fn message(id: &str, reply_to: Option<&str>) -> TeamMessage {
    TeamMessage {
        id: id.into(),
        sender: TeamAddress {
            machine_id: "machine".into(),
            server_id: "primary".into(),
            thread_id: "thread".into(),
        },
        recipient: None,
        kind: TeamMessageKind::Board,
        body: format!("sensitive-content-{id}"),
        reply_to: reply_to.map(str::to_owned),
        topic: Some("testing".into()),
        created_at: 42,
    }
}

fn entry(message: TeamMessage, conversation_id: &str, append_seq: i64) -> TeamBoardEntry {
    TeamBoardEntry {
        message,
        conversation_id: conversation_id.into(),
        append_seq,
        author: TeamBoardAuthor {
            identity: TeamBoardIdentity::User,
            name: "User".into(),
            machine_name: None,
            legacy: false,
        },
        mentions: Vec::new(),
        notifications: Vec::new(),
    }
}

async fn seed(store: &TeamStore) -> Result<()> {
    for (id, parent, conversation, seq) in [
        ("root", None, "root", 1),
        ("reply", Some("root"), "root", 2),
        ("other", None, "other", 3),
    ] {
        let message = message(id, parent);
        let record = json!({"message":message,"delivery":"nextTurn","targets":[],"receipts":{},"published":true,"error":null,"externalUrl":null});
        store
            .put(
                "board_entry",
                id,
                &entry(message.clone(), conversation, seq),
            )
            .await?;
        store.put("message", id, &record).await?;
        store
            .put(
                "operator_result",
                &format!("retry-{id}"),
                &json!({"type":"message","message":record}),
            )
            .await?;
    }
    store
        .put(
            "board_clock",
            "current",
            &json!({"revision":7,"appendSeq":3}),
        )
        .await?;
    store
        .put("board_topic", "topic-follow", &json!({"keep":true}))
        .await?;
    store
        .put(
            "board_follower",
            "root-follow",
            &json!({"conversationId":"root"}),
        )
        .await?;
    store
        .put("board_read", "root-read", &json!({"conversationId":"root"}))
        .await?;
    store.put("rejected_outbox", "rejected", &json!({"entry":{"id":"root","kind":"message","payload":{"message":message("root",None)}},"reason":"sensitive-content-root"})).await?;
    Ok(())
}

fn commit(scope: TeamBoardPurgeScope, revision: i64, id: &str) -> BoardPurgeCommit {
    BoardPurgeCommit {
        scope,
        expected_revision: revision,
        operation_id: id.into(),
        digest: json!(format!("digest-{id}")).to_string(),
        workers: vec![TeamBoardPurgeWorker {
            machine_id: "machine".into(),
            server_id: "primary".into(),
            installation_id: "installation".into(),
            acknowledged: false,
        }],
    }
}

#[tokio::test]
async fn purge_erases_all_coordinator_copies_and_replays_frozen_result() -> Result<()> {
    let (_home, store) = fixture().await?;
    seed(&store).await?;
    let scope = TeamBoardPurgeScope::Conversation {
        conversation_id: "root".into(),
    };
    assert_eq!(
        store.preview_board_purge(&scope).await?,
        TeamBoardPurgePreview {
            scope: scope.clone(),
            revision: 7,
            conversation_count: 1,
            message_count: 2,
        }
    );
    let operation = store.purge_board(commit(scope.clone(), 7, "purge")).await?;
    assert_eq!(operation.message_count, 2);
    assert_eq!(store.board_clock().await?.append_seq, 3);
    assert!(store.board_message_removed("root").await?);
    assert!(store.board_conversation_removed("root").await?);
    assert_eq!(store.board_entries(None, 0).await?.len(), 1);
    assert_eq!(
        store.get::<Value>("board_follower", "root-follow").await?,
        None
    );
    assert_eq!(
        store.get::<Value>("board_topic", "topic-follow").await?,
        Some(json!({"keep":true}))
    );

    let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM record")
        .fetch_all(&store.pool)
        .await?;
    assert!(
        rows.iter()
            .all(|row| !row.contains("sensitive-content-root")
                && !row.contains("sensitive-content-reply"))
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("sensitive-content-other"))
    );
    assert_eq!(
        store.get::<Value>("operator_result", "retry-root").await?,
        Some(json!({"type":"removed","messageId":"root"}))
    );

    // A stale revision is valid only when recovering the exact accepted operation.
    assert_eq!(
        store.purge_board(commit(scope.clone(), 7, "purge")).await?,
        operation
    );
    assert!(
        store
            .acknowledge_board_purges("installation", &["purge".into()])
            .await?
    );
    assert!(
        !store
            .acknowledge_board_purges("installation", &["purge".into()])
            .await?
    );
    assert!(store.purge_board(commit(scope, 7, "purge")).await?.workers[0].acknowledged);
    Ok(())
}

#[tokio::test]
async fn stale_preview_and_conflicting_global_identity_do_not_change_content() -> Result<()> {
    let (_home, store) = fixture().await?;
    seed(&store).await?;
    let before = store.board_entries(None, 0).await?;
    assert!(
        store
            .purge_board(commit(TeamBoardPurgeScope::All, 6, "stale"))
            .await
            .is_err()
    );
    store
        .put("operator_operation", "taken", &"another-digest")
        .await?;
    assert!(
        store
            .purge_board(commit(TeamBoardPurgeScope::All, 7, "taken"))
            .await
            .is_err()
    );
    assert_eq!(store.board_entries(None, 0).await?, before);
    assert!(store.board_purge_journal().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn topic_and_all_scopes_include_whole_conversations_and_allow_empty_selection() -> Result<()>
{
    let (_home, store) = fixture().await?;
    seed(&store).await?;
    let empty = store
        .purge_board(commit(
            TeamBoardPurgeScope::Topic {
                topic: "absent".into(),
            },
            7,
            "empty",
        ))
        .await?;
    assert_eq!((empty.conversation_count, empty.message_count), (0, 0));
    let topic = store
        .purge_board(commit(
            TeamBoardPurgeScope::Topic {
                topic: "testing".into(),
            },
            8,
            "topic",
        ))
        .await?;
    assert_eq!((topic.conversation_count, topic.message_count), (2, 3));
    assert_eq!(
        store
            .preview_board_purge(&TeamBoardPurgeScope::All)
            .await?
            .message_count,
        0
    );
    Ok(())
}

#[tokio::test]
async fn worker_purge_erases_acknowledged_outbox_cache_and_delivery_without_recall() -> Result<()> {
    let (_home, store) = fixture().await?;
    let message = message("root", None);
    let snapshot = TeamBoardSnapshot {
        revision: 7,
        entries: vec![entry(message.clone(), "root", 1)],
        ..Default::default()
    };
    store.put("board_cache", "current", &snapshot).await?;
    store.put("board:testing", "root", &message).await?;
    store
        .enqueue("root", "message", &json!({"message":message}))
        .await?;
    let source = store.outbox(0, 10).await?;
    store.acknowledge(&[source[0].sequence]).await?;
    store.enqueue("queued-reply", "board", &json!({"request":{"type":"reply","conversationId":"root","body":"sensitive-content-queued"}})).await?;
    let delivery = TeamDelivery {
        id: "delivery".into(),
        thread_id: "thread".into(),
        message: message.clone(),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
    };
    store.receive(&delivery).await?;
    let admitted = TeamDelivery {
        id: "admitted".into(),
        ..delivery.clone()
    };
    let mut receipt = store.receive(&admitted).await?;
    receipt.state = TeamReceiptState::Accepted;
    receipt.turn_id = Some("turn".into());
    store.update_receipt(&receipt).await?;
    let journal = TeamBoardPurgeJournal {
        operation_id: "purge".into(),
        revision: 8,
        conversation_ids: vec!["root".into()],
        message_ids: vec!["root".into()],
    };
    assert_eq!(
        store
            .apply_board_purges(std::slice::from_ref(&journal))
            .await?,
        vec!["purge"]
    );
    assert_eq!(store.apply_board_purges(&[journal]).await?, vec!["purge"]);
    let deliveries = store.deliveries("thread").await?;
    assert_eq!(deliveries[0].1.state, TeamReceiptState::Rejected);
    assert_eq!(
        deliveries[0].1.detail.as_deref(),
        Some("Removed before delivery.")
    );
    assert_eq!(deliveries[1].1.state, TeamReceiptState::Accepted);
    assert!(
        deliveries
            .iter()
            .all(|(delivery, _)| delivery.message.body.is_empty())
    );
    assert!(store.message_outbox_evidence().await?.is_empty());
    assert!(
        store
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(store.get::<Value>("board:testing", "root").await?.is_none());
    let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM record UNION ALL SELECT payload FROM delivery UNION ALL SELECT payload FROM outbox").fetch_all(&store.pool).await?;
    assert!(rows.iter().all(|row| !row.contains("sensitive-content")));

    // Neither a stale delivery nor a new reply can restore the erased body.
    assert_eq!(
        store.receive(&delivery).await?.state,
        TeamReceiptState::Rejected
    );
    assert_eq!(
        store
            .receive(&TeamDelivery {
                id: "late".into(),
                ..delivery
            })
            .await?
            .state,
        TeamReceiptState::Rejected
    );
    assert!(
        store
            .enqueue(
                "late-reply",
                "board",
                &json!({"request":{"conversationId":"root","body":"late"}})
            )
            .await
            .is_err()
    );
    let mut stale = snapshot;
    store.filter_removed_board_snapshot(&mut stale).await?;
    assert!(stale.entries.is_empty());
    Ok(())
}

#[tokio::test]
async fn readonly_preview_does_not_create_missing_store_or_change_existing_rows() -> Result<()> {
    let (home, store) = fixture().await?;
    seed(&store).await?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let missing = home.path().join("missing.sqlite");
    assert!(TeamStore::open_read_only(&sqlite, &missing).await.is_err());
    assert!(!missing.exists());
    let readonly = TeamStore::open_read_only(&sqlite, &home.path().join("team.sqlite")).await?;
    assert_eq!(
        readonly
            .preview_board_purge(&TeamBoardPurgeScope::All)
            .await?
            .message_count,
        3
    );
    assert_eq!(readonly.board_clock().await?.revision, 7);
    assert!(readonly.board_purge_journal().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn purge_migration_preserves_content_and_fences_protocol_four_reader() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let path = home.path().join("team.sqlite");
    let pool = sqlite.open_read_write_pool(&path).await?;
    let current = sqlx::migrate!("./migrations");
    let old = sqlx::migrate::Migrator::with_migrations(
        current
            .iter()
            .filter(|migration| migration.version <= 4)
            .cloned()
            .collect(),
    );
    old.run(&pool).await?;
    sqlx::query("INSERT INTO record(namespace,id,payload) VALUES ('message','preserved',?)")
        .bind(json!({"body":"keep through schema upgrade"}).to_string())
        .execute(&pool)
        .await?;
    let store = TeamStore::open(&sqlite, &path).await?;
    assert_eq!(
        store.get::<Value>("message", "preserved").await?,
        Some(json!({"body":"keep through schema upgrade"}))
    );
    assert!(matches!(
        old.run(&pool).await,
        Err(sqlx::migrate::MigrateError::VersionMissing(5))
    ));
    Ok(())
}

#[tokio::test]
async fn rejected_future_revision_cannot_become_eligible_on_retry() -> Result<()> {
    let (_home, store) = fixture().await?;
    seed(&store).await?;
    assert!(
        store
            .purge_board(commit(TeamBoardPurgeScope::All, 8, "future"))
            .await
            .is_err()
    );
    store
        .put(
            "board_clock",
            "current",
            &json!({"revision":8,"appendSeq":3}),
        )
        .await?;
    assert!(
        store
            .purge_board(commit(TeamBoardPurgeScope::All, 8, "future"))
            .await
            .is_err()
    );
    assert_eq!(store.board_entries(None, 0).await?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn whole_board_retry_after_restart_never_erases_later_posts() -> Result<()> {
    let (home, store) = fixture().await?;
    seed(&store).await?;
    let original = store
        .purge_board(commit(TeamBoardPurgeScope::All, 7, "all"))
        .await?;
    assert_eq!(
        (original.conversation_count, original.message_count),
        (2, 3)
    );
    let later = entry(message("later", None), "later", 4);
    store.put("board_entry", "later", &later).await?;
    store
        .put(
            "board_clock",
            "current",
            &json!({"revision":9,"appendSeq":4}),
        )
        .await?;
    store.pool.close().await;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let restarted = TeamStore::open(&sqlite, &home.path().join("team.sqlite")).await?;
    assert_eq!(
        restarted
            .purge_board(commit(TeamBoardPurgeScope::All, 7, "all"))
            .await?,
        original
    );
    assert_eq!(restarted.board_entries(None, 0).await?, vec![later]);
    assert!(restarted.board_message_removed("root").await?);
    Ok(())
}

#[tokio::test]
async fn worker_purge_erases_transitive_queued_replies_before_acknowledging() -> Result<()> {
    let (_home, store) = fixture().await?;
    // Descendants deliberately precede their parents in durable source order.
    store
        .enqueue(
            "second",
            "message",
            &json!({"message":message("second",Some("first"))}),
        )
        .await?;
    store.enqueue("third", "board", &json!({"request":{"type":"reply","conversationId":"second","body":"sensitive-content-third"}})).await?;
    let acknowledged = store.outbox(0, 10).await?;
    store.acknowledge(&[acknowledged[1].sequence]).await?;
    store
        .enqueue(
            "first",
            "message",
            &json!({"message":message("first",Some("root"))}),
        )
        .await?;
    store
        .enqueue(
            "other",
            "message",
            &json!({"message":message("other",None)}),
        )
        .await?;
    let delivery = TeamDelivery {
        id: "queued-delivery".into(),
        thread_id: "thread".into(),
        message: message("delivery-child", Some("third")),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
    };
    store.receive(&delivery).await?;
    store
        .put("board:testing", "second", &message("second", Some("first")))
        .await?;
    store
        .put(
            "board_cache",
            "current",
            &TeamBoardSnapshot {
                revision: 7,
                entries: vec![entry(message("second", Some("first")), "root", 2)],
                ..Default::default()
            },
        )
        .await?;

    let journal = TeamBoardPurgeJournal {
        operation_id: "purge".into(),
        revision: 8,
        conversation_ids: vec!["root".into()],
        message_ids: vec!["root".into()],
    };
    assert_eq!(store.apply_board_purges(&[journal]).await?, vec!["purge"]);
    for id in ["root", "first", "second", "third", "delivery-child"] {
        assert!(
            store.board_message_removed(id).await?,
            "missing descendant marker {id}"
        );
    }
    assert_eq!(
        store
            .message_outbox_evidence()
            .await?
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec!["other"]
    );
    let delivered = store.deliveries("thread").await?;
    assert_eq!(delivered[0].1.state, TeamReceiptState::Rejected);
    assert!(delivered[0].0.message.body.is_empty());
    let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM record UNION ALL SELECT payload FROM delivery UNION ALL SELECT payload FROM outbox")
        .fetch_all(&store.pool).await?;
    for erased in ["first", "second", "third", "delivery-child"] {
        assert!(
            rows.iter()
                .all(|row| !row.contains(&format!("sensitive-content-{erased}"))),
            "retained descendant body {erased}"
        );
    }
    assert!(
        rows.iter()
            .any(|row| row.contains("sensitive-content-other"))
    );
    assert!(
        store
            .enqueue(
                "late",
                "message",
                &json!({"message":message("late",Some("third"))})
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn coordinator_purge_erases_previously_rejected_reply_chains_and_journals_their_ids()
-> Result<()> {
    let (_home, store) = fixture().await?;
    seed(&store).await?;
    for (id, parent) in [("second", "first"), ("first", "root")] {
        store.put("rejected_outbox", &format!("installation/{id}"), &json!({
            "entry":{"id":id,"kind":"message","payload":{"message":message(id,Some(parent))}},
            "reason":"Invalid when disconnected",
        })).await?;
    }
    // A rejected reuse of another accepted post's identity must not let its
    // claimed parent remove that unrelated conversation.
    store.put("rejected_outbox", "installation/conflict", &json!({
        "entry":{"id":"other","kind":"message","payload":{"message":message("other",Some("root"))}},
        "reason":"conflicting message identity",
    })).await?;
    let operation = store
        .purge_board(commit(
            TeamBoardPurgeScope::Conversation {
                conversation_id: "root".into(),
            },
            7,
            "purge",
        ))
        .await?;
    // Preview counts describe accepted Board posts, while the frozen journal
    // also covers any rejected descendants found during permanent removal.
    assert_eq!(operation.message_count, 2);
    assert_eq!(
        store.board_purge_journal().await?[0].message_ids,
        vec!["first", "reply", "root", "second"]
    );
    let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM record")
        .fetch_all(&store.pool)
        .await?;
    assert!(
        rows.iter()
            .all(|row| !row.contains("sensitive-content-first")
                && !row.contains("sensitive-content-second"))
    );
    assert!(store.board_message_removed("second").await?);
    assert!(!store.board_message_removed("other").await?);
    assert_eq!(
        store
            .board_entries(None, 0)
            .await?
            .into_iter()
            .map(|entry| entry.message.id)
            .collect::<Vec<_>>(),
        vec!["other"]
    );
    Ok(())
}
