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
