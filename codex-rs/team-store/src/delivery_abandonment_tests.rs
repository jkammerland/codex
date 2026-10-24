use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

use crate::TeamStore;

async fn seed() -> anyhow::Result<(tempfile::TempDir, TeamStore, TeamDeliveryAbandonment)> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let store = TeamStore::open(&sqlite, &home.path().join("host.sqlite")).await?;
    let delivery: TeamDelivery = serde_json::from_value(serde_json::json!({
        "id":"delivery", "threadId":"thread", "mode":"safeBoundary", "wake":true,
        "message":{
            "id":"message", "sender":{"machineId":"machine", "serverId":"server", "threadId":"thread"},
            "recipient":null, "kind":"request", "body":"original input", "replyTo":null,
            "topic":null, "createdAt":1,
        },
    }))?;
    let receipt = TeamReceipt {
        state: TeamReceiptState::NeedsReconciliation,
        turn_id: Some("turn".into()),
        detail: Some("Participating turn failed: provider error".into()),
        ..store.receive(&delivery).await?
    };
    store.update_receipt(&receipt).await?;

    // Unusual whitespace is intentional: recovery preserves stored bytes,
    // including evidence fields unknown to the current host.
    sqlx::query(
        "INSERT INTO record(namespace, id, payload) VALUES ('delivery_attempt', 'delivery', ?)",
    )
    .bind("{ \"original\": true, \"futureEvidence\": [1, 2] }")
    .execute(&store.pool)
    .await?;
    let abandonment = TeamDeliveryAbandonment {
        operation_id: "019a0000-0000-7000-8000-000000000001".into(),
        delivery_id: delivery.id,
        thread_id: delivery.thread_id,
        reason: "Reviewed original failure; remove without replay".into(),
    };
    Ok((home, store, abandonment))
}

#[tokio::test]
async fn abandonment_commits_once_and_preserves_original_receipt_and_attempt_bytes()
-> anyhow::Result<()> {
    let (_home, store, abandonment) = seed().await?;
    let original: (String, String, String) = sqlx::query_as("SELECT delivery.payload, delivery.receipt, record.payload FROM delivery JOIN record ON record.namespace = 'delivery_attempt' AND record.id = delivery.id WHERE delivery.id = 'delivery'")
        .fetch_one(&store.pool).await?;
    let expected = TeamDeliveryAbandonResponse {
        abandonment: abandonment.clone(),
    };
    assert_eq!(
        store
            .abandon_delivery_once(&abandonment, "coordinator:operation", "digest")
            .await?,
        expected
    );
    assert_eq!(
        store
            .abandon_delivery_once(&abandonment, "coordinator:operation", "digest")
            .await?,
        expected
    );
    assert_eq!(
        store
            .host_operation_result::<TeamDeliveryAbandonResponse>("coordinator:operation", "digest")
            .await?,
        Some(expected.clone())
    );
    let unchanged: (String, String, String) = sqlx::query_as("SELECT delivery.payload, delivery.receipt, record.payload FROM delivery JOIN record ON record.namespace = 'delivery_attempt' AND record.id = delivery.id WHERE delivery.id = 'delivery'")
        .fetch_one(&store.pool).await?;
    assert_eq!(unchanged, original);
    assert_eq!(
        store
            .get::<TeamDeliveryAbandonment>("delivery_abandonment", "delivery")
            .await?,
        Some(abandonment.clone())
    );
    let outbox = store
        .outbox(0, 100)
        .await?
        .into_iter()
        .filter(|entry| entry.kind == "delivery_abandonment")
        .collect::<Vec<_>>();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].payload, serde_json::to_value(&abandonment)?);
    assert!(
        store
            .abandon_delivery_once(&abandonment, "coordinator:operation", "different")
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting content")
    );
    assert!(
        store
            .abandon_delivery_once(&abandonment, "coordinator:different-operation", "digest")
            .await
            .unwrap_err()
            .to_string()
            .contains("already has")
    );
    assert_eq!(
        store
            .host_operation_result::<TeamDeliveryAbandonResponse>(
                "coordinator:different-operation",
                "digest"
            )
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn failed_publication_rolls_back_disposition_and_operation_result() -> anyhow::Result<()> {
    let (_home, store, abandonment) = seed().await?;
    let id = format!("abandonment:{}", abandonment.operation_id);
    store
        .enqueue(&id, "different-kind", &serde_json::json!({}))
        .await?;
    let before = store.deliveries("thread").await?;
    assert!(
        store
            .abandon_delivery_once(&abandonment, "coordinator:operation", "digest")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get::<TeamDeliveryAbandonment>("delivery_abandonment", "delivery")
            .await?,
        None
    );
    assert_eq!(
        store
            .host_operation_result::<TeamDeliveryAbandonResponse>("coordinator:operation", "digest")
            .await?,
        None
    );
    assert_eq!(store.deliveries("thread").await?, before);
    assert_eq!(
        store
            .outbox(0, 100)
            .await?
            .into_iter()
            .filter(|entry| entry.kind == "delivery_abandonment")
            .count(),
        0
    );
    Ok(())
}
