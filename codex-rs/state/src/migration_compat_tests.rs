use std::borrow::Cow;

use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::AssertSqlSafe;
use sqlx::SqlSafeStr;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::MigrationType;
use sqlx::migrate::Migrator;

use super::alternate_line_endings;
use super::run_migrations;
use crate::SqliteConfig;

const CREATE: &str = "-- The original migration must never run twice.\nCREATE TABLE kept (value TEXT);\nINSERT INTO kept VALUES ('original');\n";
type MigrationRecord = (i64, String, String, bool, Vec<u8>, i64);

fn migration(version: i64, sql: String) -> Migration {
    Migration::new(
        version,
        format!("migration {version}").into(),
        MigrationType::Simple,
        AssertSqlSafe(sql).into_sql_str(),
        false,
    )
}

async fn records(pool: &SqlitePool) -> Vec<MigrationRecord> {
    sqlx::query_as(
        "SELECT version, description, installed_on, success, checksum, execution_time FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn database() -> (impl Drop, SqliteConfig, SqlitePool) {
    let path = crate::runtime::test_support::unique_temp_dir();
    std::fs::create_dir_all(&path).unwrap();
    let cleanup = scopeguard::guard(path.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = SqliteConfig::new_for_testing(path.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    (cleanup, sqlite, pool)
}

async fn upgrade_preserves_history(old_crlf: bool) {
    let (_cleanup, sqlite, pool) = database().await;
    let old_sql = if old_crlf {
        CREATE.replace('\n', "\r\n")
    } else {
        CREATE.into()
    };
    let mut old = Migrator::with_migrations(vec![migration(1, old_sql.clone())]);
    old.ignore_missing = true;
    old.run(&pool).await.unwrap();
    sqlx::query("UPDATE kept SET value = 'user data'")
        .execute(&pool)
        .await
        .unwrap();
    let before = records(&pool).await;
    let current = Migrator::with_migrations(vec![
        migration(1, alternate_line_endings(&old_sql).unwrap()),
        migration(2, "CREATE TABLE added (value TEXT);\n".into()),
    ]);

    // The original strict migrator reproduces the released startup failure.
    assert!(matches!(
        current.run(&pool).await,
        Err(sqlx::migrate::MigrateError::VersionMismatch(1))
    ));
    pool.close().await;
    let reopened = sqlite
        .open_state_db(&current, /*telemetry_override*/ None)
        .await
        .unwrap();
    let after = records(&reopened).await;
    assert_eq!(&after[..1], before.as_slice());
    assert_eq!(after.len(), 2);
    assert_eq!(after[1].4, current.migrations[1].checksum.as_ref());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM kept")
            .fetch_all(&reopened)
            .await
            .unwrap(),
        vec!["user data".to_string()]
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'added'",
        )
        .fetch_one(&reopened)
        .await
        .unwrap(),
        "added"
    );

    // Both the retained binary's checksums and another new open still work.
    old.run(&reopened).await.unwrap();
    run_migrations(&reopened, &current).await.unwrap();
    assert_eq!(records(&reopened).await, after);
    reopened.close().await;
}

#[tokio::test]
async fn lf_build_opens_crlf_history_without_rewriting_or_replaying() {
    upgrade_preserves_history(/*old_crlf*/ true).await;
}

#[tokio::test]
async fn crlf_build_opens_lf_history_without_rewriting_or_replaying() {
    upgrade_preserves_history(/*old_crlf*/ false).await;
}

#[tokio::test]
async fn unknown_sql_changes_still_reject_without_new_migrations() {
    let (_cleanup, _sqlite, pool) = database().await;
    let old = Migrator::with_migrations(vec![migration(1, CREATE.into())]);
    old.run(&pool).await.unwrap();
    let before = records(&pool).await;
    let changed = Migrator::with_migrations(vec![
        migration(1, CREATE.replace("original", "changed")),
        migration(2, "CREATE TABLE forbidden (value TEXT);\n".into()),
    ]);
    let error = run_migrations(&pool, &changed).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<sqlx::migrate::MigrateError>(),
        Some(sqlx::migrate::MigrateError::VersionMismatch(1))
    ));
    assert_eq!(records(&pool).await, before);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM kept")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "original"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'forbidden'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn changed_newlines_in_sql_values_still_reject() {
    let (_cleanup, _sqlite, pool) = database().await;
    let sql = "CREATE TABLE kept (value TEXT);\nINSERT INTO kept VALUES ('a\nb');\n";
    Migrator::with_migrations(vec![migration(1, sql.into())])
        .run(&pool)
        .await
        .unwrap();
    let before = records(&pool).await;
    let changed = Migrator::with_migrations(vec![migration(1, sql.replace('\n', "\r\n"))]);
    let error = run_migrations(&pool, &changed).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<sqlx::migrate::MigrateError>(),
        Some(sqlx::migrate::MigrateError::VersionMismatch(1))
    ));
    assert_eq!(records(&pool).await, before);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM kept")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "a\nb"
    );
    pool.close().await;
}

#[test]
fn newline_compatibility_only_changes_sql_whitespace_and_comments() {
    let sql = "-- 'quote in comment\n/* another \"quote\nand newline */\nSELECT 'it''s', \"a\"\"b\", `a``b`, [name];\n";
    assert_eq!(alternate_line_endings(sql), Some(sql.replace('\n', "\r\n")));
    for sql in [
        "SELECT 'a\nb';\n",
        "SELECT \"a\nb\";\n",
        "SELECT `a\nb`;\n",
        "SELECT [a\nb];\n",
        "SELECT 1;\rSELECT 2;\n",
        "SELECT 1;\r\nSELECT 2;\n",
        "SELECT 'unclosed\n",
        "/* unclosed\n",
    ] {
        assert_eq!(alternate_line_endings(sql), None, "{sql:?}");
    }
}

#[tokio::test]
async fn runtime_and_history_open_actual_crlf_migration_families() {
    let (_cleanup, sqlite, initial_pool) = database().await;
    initial_pool.close().await;
    let families = [
        (sqlite.state_db_path(), &crate::migrations::STATE_MIGRATOR),
        (sqlite.logs_db_path(), &crate::migrations::LOGS_MIGRATOR),
        (sqlite.goals_db_path(), &crate::migrations::GOALS_MIGRATOR),
        (
            sqlite.memories_db_path(),
            &crate::migrations::MEMORIES_MIGRATOR,
        ),
        (sqlite.queue_db_path(), &crate::migrations::QUEUE_MIGRATOR),
        (
            sqlite.thread_history_db_path(),
            &crate::migrations::THREAD_HISTORY_MIGRATOR,
        ),
    ];
    let mut before = Vec::new();
    for (path, source) in &families {
        assert!(!source.migrations.is_empty());
        let old = Migrator {
            migrations: Cow::Owned(
                source
                    .migrations
                    .iter()
                    .map(|item| {
                        let mut old =
                            migration(item.version, item.sql.as_str().replace('\n', "\r\n"));
                        old.description = item.description.clone();
                        old
                    })
                    .collect(),
            ),
            ..Migrator::DEFAULT
        };
        let pool = sqlite.open_read_write_pool(path).await.unwrap();
        old.run(&pool).await.unwrap();
        before.push(records(&pool).await);
        pool.close().await;
    }

    let runtime = crate::StateRuntime::init(sqlite.clone(), "test-provider".into())
        .await
        .unwrap();
    runtime.close().await;
    let history = sqlite
        .open_thread_history_db(&crate::migrations::THREAD_HISTORY_MIGRATOR, None)
        .await
        .unwrap();
    history.close().await;
    for ((path, _), before) in families.iter().zip(before) {
        let pool = sqlite.open_read_only_pool(path, None).await.unwrap();
        assert_eq!(records(&pool).await, before, "{}", path.display());
        pool.close().await;
    }
}

#[tokio::test]
async fn crlf_recency_migration_keeps_its_checksum_when_relocated() {
    let (_cleanup, sqlite, pool) = database().await;
    let source = &crate::migrations::STATE_MIGRATOR;
    let recency = source.iter().find(|item| item.version == 39).unwrap();
    let mut migrations = source
        .iter()
        .filter(|item| item.version < 38)
        .cloned()
        .collect::<Vec<_>>();
    let mut legacy = migration(38, recency.sql.as_str().replace('\n', "\r\n"));
    legacy.description = recency.description.clone();
    migrations.push(legacy);
    Migrator::with_migrations(migrations)
        .run(&pool)
        .await
        .unwrap();
    let before = records(&pool).await;
    let mut expected = before.last().unwrap().clone();
    assert_eq!(expected.0, 38);
    expected.0 = 39;
    pool.close().await;

    let reopened = sqlite.open_state_db(source, None).await.unwrap();
    let after = records(&reopened).await;
    assert_eq!(&after[..before.len() - 1], &before[..before.len() - 1]);
    assert_eq!(after.iter().find(|row| row.0 == 39), Some(&expected));
    reopened.close().await;
}
