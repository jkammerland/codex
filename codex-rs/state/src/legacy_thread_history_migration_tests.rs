use std::borrow::Cow;

use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::MigrationType;
use sqlx::migrate::Migrator;

use super::THREAD_HISTORY_MIGRATOR;
use super::runtime_thread_history_migrator;
use crate::SqliteConfig;

// Exact shipped SQL from fork commit 8e50037a30. Reapplying it would erase
// projections that have accumulated since that migration originally ran.
const LEGACY_SQL: &str = "-- Terminal agent status markers now correct the paginated thread-history view. Rebuild the\n\
-- disposable projection so markers already passed by the old checkpoint are applied.\n\
DELETE FROM thread_items;\n\
DELETE FROM thread_turns;\n\
DELETE FROM thread_history_projection_state;\n";

async fn history_before_seven() -> (impl Drop, SqliteConfig, SqlitePool) {
    let path = crate::runtime::test_support::unique_temp_dir();
    std::fs::create_dir_all(&path).unwrap();
    let cleanup = scopeguard::guard(path.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::new_for_testing(path.as_path().abs());
    let migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 7)
                .cloned()
                .collect(),
        ),
        ..runtime_thread_history_migrator()
    };
    let pool = sqlite
        .open_thread_history_db(&migrator, /*telemetry_override*/ None)
        .await
        .unwrap();
    (cleanup, sqlite, pool)
}

async fn apply_legacy_seven(pool: &SqlitePool, sql: &'static str) {
    let mut migrator = runtime_thread_history_migrator();
    migrator.migrations = Cow::Owned(vec![Migration::new(
        /*version*/ 7,
        "reproject terminal agent status".into(),
        MigrationType::Simple,
        sqlx::SqlStr::from_static(sql),
        /*no_tx*/ false,
    )]);
    migrator.run(pool).await.unwrap();
}

#[tokio::test]
async fn legacy_history_seven_is_relocated_without_replaying_or_losing_rows() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, LEGACY_SQL).await;
    sqlx::raw_sql(
        "INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status)
         VALUES ('thread', 'turn', 10, 'completed');
         INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json)
         VALUES ('thread', 'turn', 'item', 11, 100, '{\"text\":\"preserve me\"}');
         INSERT INTO thread_history_projection_state VALUES ('thread', 1234, 12);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let legacy_record = sqlx::query_as::<_, (String, String, bool, Vec<u8>, i64)>(
        "SELECT description, installed_on, success, checksum, execution_time FROM _sqlx_migrations WHERE version = 7",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let error = runtime_thread_history_migrator()
        .run(&pool)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("migration 7 was previously applied but has been modified")
    );
    pool.close().await;

    let repaired = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .expect("normal history open repairs the known fork migration collision");
    let relocated_record = sqlx::query_as::<_, (String, String, bool, Vec<u8>, i64)>(
        "SELECT description, installed_on, success, checksum, execution_time FROM _sqlx_migrations WHERE version = 202608300007",
    )
    .fetch_one(&repaired)
    .await
    .unwrap();
    assert_eq!(relocated_record, legacy_record);
    let item = sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
        "SELECT item_json, started_at_ms, completed_at_ms FROM thread_items WHERE item_id = 'item'",
    )
    .fetch_one(&repaired)
    .await
    .unwrap();
    assert_eq!(item, ("{\"text\":\"preserve me\"}".into(), None, None));
    assert_eq!(
        sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT status, next_rollout_byte_offset, next_rollout_ordinal FROM thread_turns JOIN thread_history_projection_state USING (thread_id)",
        )
        .fetch_one(&repaired)
        .await
        .unwrap(),
        ("completed".into(), 1234, 12)
    );
    repaired.close().await;
    let reopened = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .expect("repair is idempotent");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM thread_items")
            .fetch_one(&reopened)
            .await
            .unwrap(),
        1
    );
    reopened.close().await;
    let old_migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 7)
                .cloned()
                .collect(),
        ),
        ..runtime_thread_history_migrator()
    };
    let older = sqlite
        .open_thread_history_db(&old_migrator, /*telemetry_override*/ None)
        .await
        .expect("the retained six-migration binary still opens repaired history");
    sqlx::query("INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES ('thread', 'turn', 'older-writer', 12, 101, '{}')")
        .execute(&older).await.unwrap();
    assert_eq!(
        sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
            "SELECT started_at_ms, completed_at_ms FROM thread_items WHERE item_id = 'older-writer'"
        )
        .fetch_one(&older)
        .await
        .unwrap(),
        (None, None)
    );
    older.close().await;
}

#[tokio::test]
async fn legacy_history_repair_can_run_in_a_spawned_task() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, LEGACY_SQL).await;
    pool.close().await;
    tokio::spawn(async move {
        let repaired = sqlite
            .open_thread_history_db(
                &runtime_thread_history_migrator(),
                /*telemetry_override*/ None,
            )
            .await
            .unwrap();
        repaired.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unknown_history_seven_is_still_rejected() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, "SELECT 1;").await;
    let before = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let error = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("migration 7 was previously applied but has been modified")
    );
    let after = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    pool.close().await;
}

#[tokio::test]
async fn upstream_history_seven_is_unchanged_on_reopen() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    THREAD_HISTORY_MIGRATOR.run(&pool).await.unwrap();
    let before = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    let reopened = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .unwrap();
    let after = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&reopened)
    .await
    .unwrap();
    assert_eq!(after, before);
    reopened.close().await;
}

#[tokio::test]
async fn failed_upstream_migration_rolls_back_legacy_relocation() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, LEGACY_SQL).await;
    sqlx::query("ALTER TABLE thread_items ADD COLUMN started_at_ms INTEGER")
        .execute(&pool)
        .await
        .unwrap();
    let before = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let error = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("duplicate column name: started_at_ms"));
    let after = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    pool.close().await;
}

#[tokio::test]
async fn concurrent_history_opens_repair_legacy_seven_once() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, LEGACY_SQL).await;
    pool.close().await;
    let migrator = runtime_thread_history_migrator();
    let (first, second) = tokio::join!(
        sqlite.open_thread_history_db(&migrator, /*telemetry_override*/ None),
        sqlite.open_thread_history_db(&migrator, /*telemetry_override*/ None),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT version FROM _sqlx_migrations WHERE version >= 7 ORDER BY version"
        )
        .fetch_all(&first)
        .await
        .unwrap(),
        vec![7, 202608300007]
    );
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn occupied_legacy_version_is_not_overwritten() {
    let (_cleanup, sqlite, pool) = history_before_seven().await;
    apply_legacy_seven(&pool, LEGACY_SQL).await;
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (202608300007, 'another migration', 1, X'01', 0)")
        .execute(&pool).await.unwrap();
    let before = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let error = sqlite
        .open_thread_history_db(
            &runtime_thread_history_migrator(),
            /*telemetry_override*/ None,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("migration 7 was previously applied but has been modified")
    );
    let after = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    pool.close().await;
}
