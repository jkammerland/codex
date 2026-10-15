use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::TeamStore;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutboxEntry {
    pub sequence: i64,
    pub id: String,
    pub kind: String,
    pub payload: Value,
}

impl TeamStore {
    pub async fn enqueue<T: Serialize>(&self, id: &str, kind: &str, payload: &T) -> Result<()> {
        let payload = serde_json::to_string(payload)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO outbox(id, kind, payload) VALUES (?, ?, ?) ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(kind)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;
        let stored: (String, String) =
            sqlx::query_as("SELECT kind, payload FROM outbox WHERE id = ?")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            stored == (kind.to_string(), payload),
            "outbox identity has conflicting content"
        );
        tx.commit().await?;
        self.notify();
        Ok(())
    }

    pub async fn outbox(&self, after: i64, limit: u32) -> Result<Vec<OutboxEntry>> {
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as("SELECT sequence, id, kind, payload FROM outbox WHERE acknowledged = 0 AND sequence > ? ORDER BY sequence LIMIT ?")
            .bind(after).bind(limit).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|(sequence, id, kind, payload)| {
                Ok(OutboxEntry {
                    sequence,
                    id,
                    kind,
                    payload: serde_json::from_str(&payload)?,
                })
            })
            .collect()
    }

    pub async fn acknowledge(&self, sequences: &[i64]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for sequence in sequences {
            sqlx::query("UPDATE outbox SET acknowledged = 1 WHERE sequence = ?")
                .bind(sequence)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
