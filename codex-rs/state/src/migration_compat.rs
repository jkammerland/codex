//! Preserve migration history across builds made with LF or CRLF checkouts.

use std::borrow::Cow;
use std::collections::BTreeMap;

use sqlx::AssertSqlSafe;
use sqlx::SqlSafeStr;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

/// Run embedded SQLite migrations while accepting exact historical newline variants.
///
/// Only the in-memory checksum of an already-applied migration can change. Its
/// persisted record and SQL are untouched, so old binaries can still validate
/// their migration history. Unknown changes retain SQLx's checksum rejection.
pub async fn run_migrations(pool: &SqlitePool, migrator: &Migrator) -> anyhow::Result<()> {
    let compatible = with_legacy_newline_checksums(pool, migrator).await?;
    compatible.as_ref().unwrap_or(migrator).run(pool).await?;
    Ok(())
}

pub(crate) async fn with_legacy_newline_checksums(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<Option<Migrator>> {
    // All shipped stores use SQLx's default table. Leave custom migrators alone.
    if migrator.table_name != "_sqlx_migrations"
        || sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
        )
        .fetch_optional(pool)
        .await?
        .is_none()
    {
        return Ok(None);
    }

    let applied = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let mut migrations = None;
    for (index, migration) in migrator.migrations.iter().enumerate() {
        let Some(checksum) = applied.get(&migration.version) else {
            continue;
        };
        if migration.checksum.as_ref() != checksum
            && alternate_checksum(migration).as_deref() == Some(checksum.as_slice())
        {
            migrations.get_or_insert_with(|| migrator.migrations.to_vec())[index].checksum =
                Cow::Owned(checksum.clone());
        }
    }

    Ok(migrations.map(|migrations| Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: migrator.ignore_missing,
        locking: migrator.locking,
        no_tx: migrator.no_tx,
        table_name: migrator.table_name.clone(),
        create_schemas: migrator.create_schemas.clone(),
    }))
}

pub(crate) fn alternate_checksum(migration: &Migration) -> Option<Cow<'static, [u8]>> {
    let sql = alternate_line_endings(migration.sql.as_str())?;
    // Use SQLx's own SHA-384 calculation. This alternate SQL is never executed.
    Some(
        Migration::new(
            migration.version,
            migration.description.clone(),
            migration.migration_type,
            AssertSqlSafe(sql).into_sql_str(),
            migration.no_tx,
        )
        .checksum,
    )
}

fn alternate_line_endings(sql: &str) -> Option<String> {
    let lf = sql.replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    if lf.contains('\r') || lf == crlf || (sql != lf && sql != crlf) {
        return None;
    }

    // Newlines in SQL values or quoted identifiers can change stored data or
    // schema names. Only whitespace and comments may differ between checkouts.
    #[derive(Clone, Copy)]
    enum Context {
        Sql,
        LineComment,
        BlockComment,
        Quoted(u8),
    }
    let mut context = Context::Sql;
    let mut bytes = lf.bytes().peekable();
    while let Some(byte) = bytes.next() {
        match context {
            Context::Sql => match (byte, bytes.peek()) {
                (b'-', Some(b'-')) => {
                    bytes.next();
                    context = Context::LineComment;
                }
                (b'/', Some(b'*')) => {
                    bytes.next();
                    context = Context::BlockComment;
                }
                (b'\'' | b'"' | b'`', _) => context = Context::Quoted(byte),
                (b'[', _) => context = Context::Quoted(b']'),
                _ => {}
            },
            Context::LineComment if byte == b'\n' => context = Context::Sql,
            Context::LineComment => {}
            Context::BlockComment if byte == b'*' && bytes.peek() == Some(&b'/') => {
                bytes.next();
                context = Context::Sql;
            }
            Context::BlockComment => {}
            Context::Quoted(_) if byte == b'\n' => return None,
            Context::Quoted(quote) if byte == quote => {
                if quote != b']' && bytes.peek() == Some(&quote) {
                    bytes.next();
                } else {
                    context = Context::Sql;
                }
            }
            Context::Quoted(_) => {}
        }
    }
    match context {
        Context::Sql | Context::LineComment => Some(if sql == lf { crlf } else { lf }),
        Context::BlockComment | Context::Quoted(_) => None,
    }
}

#[cfg(test)]
#[path = "migration_compat_tests.rs"]
mod tests;
