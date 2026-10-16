use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_team_store::TeamStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn restart_preserves_control_and_rejects_stale_or_foreign_controls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let path = dir.path().join("host.sqlite");
    let store = TeamStore::open(&sqlite, &path).await?;
    let paused = TeamControlState {
        coordinator_id: "coordinator".into(),
        revision: 7,
        mode: TeamExecutionMode::Paused,
    };
    assert_eq!(store.apply_control(&paused).await?, paused);
    drop(store);

    let store = TeamStore::open(&sqlite, &path).await?;
    let stale = TeamControlState {
        revision: 6,
        mode: TeamExecutionMode::Running,
        ..paused.clone()
    };
    assert_eq!(store.apply_control(&stale).await?, paused);
    let conflicting = TeamControlState {
        mode: TeamExecutionMode::Running,
        ..paused.clone()
    };
    assert!(store.apply_control(&conflicting).await.is_err());
    let foreign = TeamControlState {
        coordinator_id: "different".into(),
        revision: 20,
        ..paused.clone()
    };
    assert!(store.apply_control(&foreign).await.is_err());
    assert_eq!(store.control().await?, Some(paused));
    Ok(())
}

#[tokio::test]
async fn delivery_and_outbox_replay_are_idempotent_across_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let path = dir.path().join("host.sqlite");
    let store = TeamStore::open(&sqlite, &path).await?;
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: "root".into(),
    };
    let enrollment = TeamEnrollment {
        address: address.clone(),
        alias: "worker".into(),
        manually_held: true,
        continuation_turn_id: Some("stopped-turn".into()),
    };
    store.enroll(&enrollment, "first-credential").await?;
    let rename = TeamEnrollment {
        alias: "renamed".into(),
        manually_held: false,
        continuation_turn_id: None,
        ..enrollment.clone()
    };
    assert_eq!(
        store.enroll(&rename, "replacement").await?,
        TeamEnrollment {
            alias: "renamed".into(),
            ..enrollment
        }
    );
    assert_eq!(
        store.credential("root").await?,
        Some("first-credential".into())
    );

    let delivery = TeamDelivery {
        id: "delivery-1".into(),
        thread_id: "root".into(),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
        message: TeamMessage {
            id: "message-1".into(),
            sender: address.clone(),
            recipient: Some(address),
            kind: TeamMessageKind::Request,
            body: "review this".into(),
            reply_to: None,
            topic: None,
            created_at: 100,
        },
    };
    let initial = store.receive(&delivery).await?;
    let accepted = TeamReceipt {
        state: TeamReceiptState::Accepted,
        turn_id: Some("stable-turn".into()),
        ..initial
    };
    store.update_receipt(&accepted).await?;
    drop(store);

    let store = TeamStore::open(&sqlite, &path).await?;
    assert_eq!(store.receive(&delivery).await?, accepted);
    let mut conflict = delivery.clone();
    conflict.message.body = "different request".into();
    assert!(store.receive(&conflict).await.is_err());
    assert_eq!(
        store.deliveries("root").await?,
        vec![(delivery, accepted.clone())]
    );
    let outbox = store.outbox(0, 10).await?;
    assert_eq!(outbox.len(), 1);
    assert_eq!(
        serde_json::from_value::<TeamReceipt>(outbox[0].payload.clone())?,
        accepted
    );
    let completed = TeamReceipt {
        state: TeamReceiptState::Completed,
        ..accepted
    };
    store.update_receipt(&completed).await?;
    // An old acknowledgement must not remove a transition committed while
    // that acknowledgement was in flight.
    store.acknowledge(&[outbox[0].sequence]).await?;
    let remaining = store.outbox(0, 10).await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        serde_json::from_value::<TeamReceipt>(remaining[0].payload.clone())?,
        completed
    );
    store.update_receipt(&completed).await?;
    store.acknowledge(&[remaining[0].sequence]).await?;
    assert!(store.outbox(0, 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn operation_effect_and_result_commit_together_and_retries_preserve_newer_values()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let path = dir.path().join("operations.sqlite");
    let store = TeamStore::open(&sqlite, &path).await?;
    let first = serde_json::json!({"state": "first"});
    let writes = [
        ("message", "stable-message", first.clone()),
        (
            "result",
            "operation",
            serde_json::json!({"id": "stable-message"}),
        ),
    ];
    assert!(
        store
            .put_record_once("operation", "operation", "digest", &writes)
            .await?
    );
    store
        .put(
            "message",
            "stable-message",
            &serde_json::json!({"state": "newer"}),
        )
        .await?;
    drop(store);
    let store = TeamStore::open(&sqlite, &path).await?;
    assert!(
        !store
            .put_record_once("operation", "operation", "digest", &writes)
            .await?
    );
    assert_eq!(
        store
            .get::<serde_json::Value>("message", "stable-message")
            .await?,
        Some(serde_json::json!({"state": "newer"}))
    );
    assert_eq!(
        store
            .get::<serde_json::Value>("result", "operation")
            .await?,
        Some(serde_json::json!({"id": "stable-message"}))
    );
    assert!(
        store
            .put_record_once("operation", "operation", "conflict", &writes)
            .await
            .is_err()
    );

    // Validation after an earlier candidate write still rolls the whole effect
    // back; no acknowledgement can name a partially committed operation.
    let invalid = [
        ("message", "partial", first),
        ("operation", "invalid", serde_json::json!(null)),
    ];
    assert!(
        store
            .put_record_once("operation", "invalid", "digest", &invalid)
            .await
            .is_err()
    );
    assert_eq!(
        store.get::<serde_json::Value>("message", "partial").await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn enrollment_and_leave_retries_do_not_undo_later_enrollment() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let store = TeamStore::open(&sqlite, &dir.path().join("enroll.sqlite")).await?;
    let enrollment = TeamEnrollment {
        address: TeamAddress {
            machine_id: "machine".into(),
            server_id: "server".into(),
            thread_id: "root".into(),
        },
        alias: "first".into(),
        manually_held: true,
        continuation_turn_id: None,
    };
    store
        .enroll_operation(&enrollment, "credential", Some(("enroll", "digest")))
        .await?;
    let renamed = TeamEnrollment {
        alias: "newer".into(),
        ..enrollment.clone()
    };
    store.enroll(&renamed, "unused").await?;
    assert_eq!(
        store
            .enroll_operation(&enrollment, "unused", Some(("enroll", "digest")))
            .await?,
        enrollment
    );
    assert_eq!(store.enrollment("root").await?, Some(renamed.clone()));
    store
        .unenroll("root", &["root".into()], Some(("leave", "leave-digest")))
        .await?;
    assert_eq!(store.get::<bool>("manual_hold", "root").await?, Some(true));
    store.enroll(&renamed, "new-credential").await?;
    store
        .unenroll("root", &["root".into()], Some(("leave", "leave-digest")))
        .await?;
    assert_eq!(store.enrollment("root").await?, Some(renamed));
    assert_eq!(
        store.credential("root").await?,
        Some("new-credential".into())
    );
    Ok(())
}

#[tokio::test]
async fn native_agent_operation_retry_returns_original_result_and_emits_once() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let store = TeamStore::open(&sqlite, &dir.path().join("agent.sqlite")).await?;
    let payload = serde_json::json!({"body": "peer request"});
    let result = serde_json::json!({"id": "message-original", "state": "queued"});
    assert_eq!(
        store
            .enqueue_agent_operation(
                "turn:call",
                "digest",
                "message-original",
                "message",
                &payload,
                &result
            )
            .await?,
        result
    );
    let retry = serde_json::json!({"id": "message-retry", "state": "queued"});
    assert_eq!(
        store
            .enqueue_agent_operation(
                "turn:call",
                "digest",
                "message-retry",
                "message",
                &payload,
                &retry
            )
            .await?,
        result
    );
    assert!(
        store
            .enqueue_agent_operation(
                "turn:call",
                "different",
                "message-conflict",
                "message",
                &payload,
                &retry
            )
            .await
            .is_err()
    );
    let outbox = store.outbox(0, 10).await?;
    assert_eq!(outbox.len(), 1);
    assert_eq!(
        (&outbox[0].id, &outbox[0].payload),
        (&"message-original".to_string(), &payload)
    );
    Ok(())
}

#[tokio::test]
async fn stop_scope_is_durable_and_stopped_enrollment_reopens_local_cleanup() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(dir.path())?);
    let store = TeamStore::open(&sqlite, &dir.path().join("scope.sqlite")).await?;
    let root = |id: &str| TeamEnrollment {
        address: TeamAddress {
            machine_id: "machine".into(),
            server_id: "server".into(),
            thread_id: id.into(),
        },
        alias: id.into(),
        manually_held: false,
        continuation_turn_id: None,
    };
    store.enroll(&root("first"), "first-credential").await?;
    let stopped = TeamControlState {
        coordinator_id: "coordinator".into(),
        revision: 2,
        mode: TeamExecutionMode::Stopped,
    };
    store.apply_control(&stopped).await?;
    let revision = "00000000000000000002";
    assert_eq!(
        store.get::<Vec<String>>("control_scope", revision).await?,
        Some(vec!["first".into()])
    );
    store.put("control_cleanup_done", revision, &true).await?;
    store.enroll(&root("second"), "second-credential").await?;
    assert_eq!(
        store.get::<Vec<String>>("control_scope", revision).await?,
        Some(vec!["first".into(), "second".into()])
    );
    assert_eq!(
        store.get::<bool>("control_cleanup_done", revision).await?,
        None
    );
    store
        .apply_control(&TeamControlState {
            revision: 3,
            mode: TeamExecutionMode::Running,
            ..stopped
        })
        .await?;
    store.enroll(&root("third"), "third-credential").await?;
    assert_eq!(
        store.get::<Vec<String>>("control_scope", revision).await?,
        Some(vec!["first".into(), "second".into()])
    );
    Ok(())
}
