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

    /// Queue an agent effect and preserve its result under the host-owned call
    /// identity. Lost tool replies can be recovered without creating new mail.
    pub async fn enqueue_agent_operation(
        &self,
        operation_id: &str,
        digest: &str,
        id: &str,
        kind: &str,
        payload: &Value,
        result: &Value,
    ) -> Result<Value> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM record WHERE namespace = 'agent_operation' AND id = ?",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["digest"].as_str() == Some(digest),
                "operation identity has conflicting content"
            );
            return Ok(previous["result"].clone());
        }
        sqlx::query("INSERT INTO outbox(id, kind, payload) VALUES (?, ?, ?)")
            .bind(id)
            .bind(kind)
            .bind(serde_json::to_string(payload)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('agent_operation', ?, ?)")
            .bind(operation_id)
            .bind(serde_json::to_string(
                &serde_json::json!({"digest": digest, "result": result}),
            )?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.notify();
        Ok(result.clone())
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

    /// Sender evidence survives intake acknowledgement: rejected entries are
    /// acknowledged too, so callers must merge explicit coordinator outcomes.
    pub async fn message_outbox_evidence(&self) -> Result<Vec<OutboxEntry>> {
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as("SELECT sequence, id, kind, payload FROM outbox WHERE kind IN ('message', 'board') ORDER BY sequence")
            .fetch_all(&self.pool).await?;
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
}
