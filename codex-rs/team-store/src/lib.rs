//! Durable host and coordinator ledgers. Acknowledgement follows a committed write.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use anyhow::ensure;
use codex_state::SqliteConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;
use tokio::sync::watch;

mod host;
mod outbox;

pub use outbox::OutboxEntry;

#[derive(Clone)]
pub struct TeamStore {
    pool: SqlitePool,
    changed: Arc<watch::Sender<i64>>,
}

impl TeamStore {
    pub async fn open(sqlite: &SqliteConfig, path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing database parent"))?;
        let mut directory = tokio::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        directory.mode(0o700);
        directory.create(parent).await?;
        // This ledger holds bound credentials and message bodies. Create new
        // files privately without changing unrelated existing directories.
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let pool = sqlite.open_read_write_pool(path).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self {
            pool,
            changed: Arc::new(watch::channel(0).0),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.changed.subscribe()
    }

    pub fn revision(&self) -> i64 {
        *self.changed.borrow()
    }

    fn notify(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    /// Store a domain record. Callers serialize operations spanning records.
    pub async fn put<T: Serialize>(&self, namespace: &str, id: &str, value: &T) -> Result<()> {
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES (?, ?, ?) ON CONFLICT(namespace, id) DO UPDATE SET payload = excluded.payload")
            .bind(namespace).bind(id).bind(serde_json::to_string(value)?)
            .execute(&self.pool).await?;
        self.notify();
        Ok(())
    }

    /// Commit related domain records under one writer admission.
    pub async fn put_records(&self, writes: &[(&str, &str, serde_json::Value)]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for (namespace, id, value) in writes {
            sqlx::query("INSERT INTO record(namespace, id, payload) VALUES (?, ?, ?) ON CONFLICT(namespace, id) DO UPDATE SET payload = excluded.payload")
                .bind(namespace).bind(id).bind(serde_json::to_string(value)?)
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.notify();
        Ok(())
    }

    /// Commit record effects and their idempotency receipt together. Reusing a
    /// logical operation with another digest is a conflict, never a second effect.
    pub async fn put_record_once(
        &self,
        receipt_namespace: &str,
        receipt_key: &str,
        digest: &str,
        writes: &[(&str, &str, serde_json::Value)],
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<String> =
            sqlx::query_scalar("SELECT payload FROM record WHERE namespace = ? AND id = ?")
                .bind(receipt_namespace)
                .bind(receipt_key)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(previous) = previous {
            ensure!(
                previous == digest,
                "operation identity has conflicting content"
            );
            return Ok(false);
        }
        for (namespace, id, value) in writes {
            ensure!(
                *namespace != receipt_namespace || *id != receipt_key,
                "operation receipt cannot also be an effect"
            );
            sqlx::query("INSERT INTO record(namespace, id, payload) VALUES (?, ?, ?) ON CONFLICT(namespace, id) DO UPDATE SET payload = excluded.payload")
                .bind(namespace).bind(id).bind(serde_json::to_string(value)?)
                .execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES (?, ?, ?)")
            .bind(receipt_namespace)
            .bind(receipt_key)
            .bind(digest)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.notify();
        Ok(true)
    }

    pub async fn get<T: DeserializeOwned>(&self, namespace: &str, id: &str) -> Result<Option<T>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT payload FROM record WHERE namespace = ? AND id = ?")
                .bind(namespace)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        value
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub async fn list<T: DeserializeOwned>(
        &self,
        namespace: &str,
        after: &str,
        limit: u32,
    ) -> Result<Vec<(String, T)>> {
        ensure!(limit > 0, "page limit must be positive");
        let values: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, payload FROM record WHERE namespace = ? AND id > ? ORDER BY id LIMIT ?",
        )
        .bind(namespace)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        values
            .into_iter()
            .map(|(id, value)| Ok((id, serde_json::from_str(&value)?)))
            .collect()
    }

    pub async fn delete(&self, namespace: &str, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM record WHERE namespace = ? AND id = ?")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.notify();
        Ok(())
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
