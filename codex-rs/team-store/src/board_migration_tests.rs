use anyhow::Result;
use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use sqlx::AssertSqlSafe;
use sqlx::SqlSafeStr;
use sqlx::migrate::MigrateError;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

use crate::TeamStore;

#[tokio::test]
async fn opening_crlf_team_store_preserves_board_rows_and_migration_history() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let path = home.path().join("team.sqlite");
    let pool = sqlite.open_read_write_pool(&path).await?;
    let all = sqlx::migrate!("./migrations");
    let old = Migrator::with_migrations(
        all.iter()
            .map(|migration| {
                Migration::new(
                    migration.version,
                    migration.description.clone(),
                    migration.migration_type,
                    AssertSqlSafe(migration.sql.as_str().replace('\n', "\r\n")).into_sql_str(),
                    migration.no_tx,
                )
            })
            .collect(),
    );
    old.run(&pool).await?;
    sqlx::query("INSERT INTO record VALUES ('board_entry', 'entry', '{\"body\":\"keep me\"}')")
        .execute(&pool)
        .await?;
    let read_records = || {
        sqlx::query_as::<_, (i64, String, String, bool, Vec<u8>, i64)>(
            "SELECT version, description, installed_on, success, checksum, execution_time FROM _sqlx_migrations ORDER BY version",
        )
    };
    let before = read_records().fetch_all(&pool).await?;
    assert!(!before.is_empty());

    let store = TeamStore::open(&sqlite, &path).await?;
    assert_eq!(
        store
            .get::<serde_json::Value>("board_entry", "entry")
            .await?,
        Some(json!({"body":"keep me"}))
    );
    assert_eq!(read_records().fetch_all(&pool).await?, before);
    old.run(&pool).await?;
    assert_eq!(read_records().fetch_all(&pool).await?, before);
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn migration_preserves_legacy_delivery_modes_and_fences_downgrade() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let path = home.path().join("team.sqlite");
    let pool = sqlite.open_read_write_pool(&path).await?;
    let all = sqlx::migrate!("./migrations");
    let old = Migrator::with_migrations(
        all.iter()
            .filter(|migration| migration.version <= 3)
            .cloned()
            .collect(),
    );
    old.run(&pool).await?;

    let user = json!({"type":"user"});
    let agent = json!({"type":"agent", "address":{"machineId":"machine", "serverId":"server", "threadId":"agent"}});
    let follower = |identity: &serde_json::Value| {
        json!({"conversationId":"root", "identity":identity,
        "author":{"identity":identity,"name":"Original","machineName":null,"legacy":false}})
    };
    let topic = |identity: &serde_json::Value| json!({"topic":"topic", "identity":identity});
    let cache = json!({"revision":7,"entries":[],"followers":[follower(&user),follower(&agent)],
        "reads":[],"topicSubscriptions":[topic(&user),topic(&agent)],"members":[]});
    for (namespace, id, value) in [
        ("board_follower", "user", follower(&user)),
        ("board_follower", "agent", follower(&agent)),
        ("board_topic", "user", topic(&user)),
        ("board_topic", "agent", topic(&agent)),
        ("board_topic", "removed", serde_json::Value::Null),
        ("board_cache", "current", cache),
    ] {
        sqlx::query("INSERT INTO record VALUES (?, ?, ?)")
            .bind(namespace)
            .bind(id)
            .bind(serde_json::to_string(&value)?)
            .execute(&pool)
            .await?;
    }
    // Persist the old serialized request exactly; migration must not invalidate
    // an uncertain operation's digest or alter its eventual legacy semantics.
    let payload = r#"{"sender":{"machineId":"machine","serverId":"server","threadId":"agent"},"request":{"type":"topicFollow","topic":"topic","identity":null,"enabled":true}}"#;
    sqlx::query("INSERT INTO outbox(id, kind, payload) VALUES ('operation', 'board', ?)")
        .bind(payload)
        .execute(&pool)
        .await?;
    let store = TeamStore::open(&sqlite, &path).await?;
    let snapshot = store.board_snapshot().await?;
    assert_eq!(
        snapshot
            .followers
            .iter()
            .map(|follower| follower.mode)
            .collect::<Vec<_>>(),
        vec![
            TeamBoardSubscriptionMode::Wake,
            TeamBoardSubscriptionMode::Watch
        ]
    );
    assert_eq!(
        snapshot
            .topic_subscriptions
            .iter()
            .map(|subscription| subscription.mode)
            .collect::<Vec<_>>(),
        vec![
            TeamBoardSubscriptionMode::Wake,
            TeamBoardSubscriptionMode::Watch
        ]
    );
    let cached = store
        .get::<TeamBoardSnapshot>("board_cache", "current")
        .await?
        .unwrap();
    assert_eq!(cached.revision, 7);
    assert_eq!(
        cached
            .followers
            .iter()
            .map(|follower| follower.mode)
            .collect::<Vec<_>>(),
        vec![
            TeamBoardSubscriptionMode::Watch,
            TeamBoardSubscriptionMode::Wake
        ]
    );
    assert_eq!(
        cached
            .topic_subscriptions
            .iter()
            .map(|subscription| subscription.mode)
            .collect::<Vec<_>>(),
        vec![
            TeamBoardSubscriptionMode::Watch,
            TeamBoardSubscriptionMode::Wake
        ]
    );
    let persisted: String = sqlx::query_scalar("SELECT payload FROM outbox WHERE id = 'operation'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(persisted, payload);
    assert!(matches!(
        old.run(&pool).await,
        Err(MigrateError::VersionMissing(4))
    ));
    Ok(())
}
