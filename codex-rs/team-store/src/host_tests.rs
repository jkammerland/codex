use std::sync::Arc;
use std::sync::Mutex;

use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use sqlx::Connection;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::TeamStore;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_reserves_the_writer_before_reading_its_previous_value() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("receipt.sqlite");
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let pool = sqlite.open_read_write_pool(&path).await?;
    let mut connection = pool.acquire().await?;
    connection
        .lock_handle()
        .await?
        .create_collation("receipt_gate", str::cmp)?;
    // A comparison gates the exact SELECT after SQLite has read its snapshot.
    // The other connection can then attempt writer admission deterministically.
    sqlx::raw_sql("CREATE TABLE delivery(id TEXT PRIMARY KEY, thread_id TEXT COLLATE receipt_gate, receipt TEXT); CREATE TABLE outbox(sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT, kind TEXT, payload TEXT, acknowledged INTEGER NOT NULL DEFAULT 0); CREATE TABLE record(namespace TEXT, id TEXT, payload TEXT);")
        .execute(&mut *connection).await?;
    let initial = TeamReceipt {
        delivery_id: "delivery".into(),
        thread_id: "thread".into(),
        state: TeamReceiptState::Pending,
        turn_id: None,
        detail: None,
    };
    sqlx::query("INSERT INTO delivery VALUES (?, ?, ?)")
        .bind(&initial.delivery_id)
        .bind(&initial.thread_id)
        .bind(serde_json::to_string(&initial)?)
        .execute(&mut *connection)
        .await?;
    let (read, at_read) = oneshot::channel();
    let (release, wait_for_release) = std::sync::mpsc::channel();
    let gate = Mutex::new(Some((read, wait_for_release)));
    connection
        .lock_handle()
        .await?
        .create_collation("receipt_gate", move |left, right| {
            if let Some((read, release)) = gate.lock().unwrap().take() {
                let _ = read.send(());
                release.recv().unwrap();
            }
            left.cmp(right)
        })?;
    // Return this configured connection before the sole receipt operation
    // acquires it; the shim pool can otherwise open another connection.
    connection.return_to_pool().await;
    drop(connection);
    let writer_pool = sqlite.open_read_write_pool(&path).await?;
    let mut writer = writer_pool.acquire().await?.detach();
    writer_pool.close().await;
    // Contention must be reported immediately after the gated read, rather
    // than waiting for the receipt transaction that the test itself holds.
    sqlx::query("PRAGMA busy_timeout = 0")
        .execute(&mut writer)
        .await?;
    let store = TeamStore {
        pool,
        changed: Arc::new(watch::channel(0).0),
    };
    let accepted = TeamReceipt {
        state: TeamReceiptState::Accepted,
        turn_id: Some("turn".into()),
        ..initial
    };
    let update = tokio::spawn({
        let store = store.clone();
        let accepted = accepted.clone();
        async move { store.update_receipt(&accepted).await }
    });
    at_read.await?;
    let contention = writer.begin_with("BEGIN IMMEDIATE").await;
    let writer_was_blocked = match contention {
        Err(error) => error
            .as_database_error()
            .is_some_and(|error| error.code().as_deref() == Some("5")),
        Ok(mut transaction) => {
            // A deferred receipt transaction allows this commit to invalidate
            // its read snapshot, reproducing SQLITE_BUSY_SNAPSHOT on upgrade.
            sqlx::query("INSERT INTO record VALUES ('writer', 'value', 'changed')")
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            false
        }
    };
    release.send(())?;
    update.await??;
    assert!(
        writer_was_blocked,
        "receipt must acquire writer admission before its SELECT"
    );
    let receipt: String = sqlx::query_scalar("SELECT receipt FROM delivery WHERE id = 'delivery'")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(serde_json::from_str::<TeamReceipt>(&receipt)?, accepted);
    assert_eq!(store.outbox(0, 10).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn legacy_host_upgrade_preserves_credentials_and_jobs_and_holds_ambiguous_completion()
-> anyhow::Result<()> {
    use sqlx::migrate::Migrate;

    let home = tempfile::tempdir()?;
    let path = home.path().join("legacy.sqlite");
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let pool = sqlite.open_read_write_pool(&path).await?;
    let mut connection = pool.acquire().await?.detach();
    pool.close().await;
    connection
        .ensure_migrations_table("_sqlx_migrations")
        .await?;
    let migrations = sqlx::migrate!("./migrations");
    let first = migrations
        .iter()
        .find(|migration| migration.version == 1)
        .unwrap();
    connection.apply("_sqlx_migrations", first).await?;
    let enrollment = TeamEnrollment {
        address: TeamAddress {
            machine_id: "machine".into(),
            server_id: "server".into(),
            thread_id: "root".into(),
        },
        alias: "worker".into(),
        manually_held: true,
        continuation_turn_id: Some("original-turn".into()),
    };
    sqlx::query("INSERT INTO enrollment VALUES (?, ?, ?)")
        .bind("root")
        .bind(serde_json::to_string(&enrollment)?)
        .bind("original-credential")
        .execute(&mut connection)
        .await?;
    let receipt = TeamReceipt {
        delivery_id: "delivery".into(),
        thread_id: "root".into(),
        state: TeamReceiptState::Completed,
        turn_id: Some("original-turn".into()),
        detail: None,
    };
    sqlx::query("INSERT INTO delivery(id, thread_id, payload, receipt) VALUES (?, ?, ?, ?)")
        .bind("delivery")
        .bind("root")
        .bind("{}")
        .bind(serde_json::to_string(&receipt)?)
        .execute(&mut connection)
        .await?;
    let job = serde_json::json!({"job": {"id": "original-job", "handle": "handle"}});
    sqlx::query("INSERT INTO record VALUES ('job', 'original-job', ?)")
        .bind(serde_json::to_string(&job)?)
        .execute(&mut connection)
        .await?;
    connection.close().await?;

    let store = TeamStore::open(&sqlite, &path).await?;
    assert_eq!(store.enrollment("root").await?, Some(enrollment));
    assert_eq!(
        store.credential("root").await?,
        Some("original-credential".into())
    );
    assert_eq!(
        store
            .get::<serde_json::Value>("job", "original-job")
            .await?,
        Some(job)
    );
    let receipt: String = sqlx::query_scalar("SELECT receipt FROM delivery WHERE id = 'delivery'")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(
        serde_json::from_str::<TeamReceipt>(&receipt)?.state,
        TeamReceiptState::NeedsReconciliation
    );
    assert_eq!(store.outbox(0, 10).await?.len(), 1);
    Ok(())
}
