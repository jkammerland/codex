//! A recovery disposition leaves the original execution evidence untouched.

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamDeliveryAbandonResponse;
use codex_app_server_protocol::TeamDeliveryAbandonment;
use codex_app_server_protocol::TeamReceipt;
use codex_app_server_protocol::TeamReceiptState;

use crate::TeamStore;

impl TeamStore {
    /// Commit the operator result, disposition and publication together. The
    /// caller fences family admission and serializes delivery evidence updates.
    pub async fn abandon_delivery_once(
        &self,
        abandonment: &TeamDeliveryAbandonment,
        operation_key: &str,
        digest: &str,
    ) -> Result<TeamDeliveryAbandonResponse> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM record WHERE namespace = 'host_operation' AND id = ?",
        )
        .bind(operation_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            let previous: serde_json::Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["digest"].as_str() == Some(digest),
                "operation identity has conflicting content"
            );
            return Ok(serde_json::from_value(previous["result"].clone())?);
        }

        let receipt: Option<String> =
            sqlx::query_scalar("SELECT receipt FROM delivery WHERE id = ? AND thread_id = ?")
                .bind(&abandonment.delivery_id)
                .bind(&abandonment.thread_id)
                .fetch_optional(&mut *tx)
                .await?;
        let receipt: TeamReceipt = serde_json::from_str(
            &receipt.ok_or_else(|| anyhow::anyhow!("delivery is not addressed to this thread"))?,
        )?;
        ensure!(
            matches!(
                receipt.state,
                TeamReceiptState::Accepted | TeamReceiptState::NeedsReconciliation
            ),
            "only an admitted delivery with an unresolved outcome may be abandoned"
        );
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM record WHERE namespace = 'delivery_abandonment' AND id = ?)",
        )
        .bind(&abandonment.delivery_id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(!exists, "delivery already has an abandonment disposition");

        let response = TeamDeliveryAbandonResponse {
            abandonment: abandonment.clone(),
        };
        let payload = serde_json::to_string(abandonment)?;
        sqlx::query(
            "INSERT INTO record(namespace, id, payload) VALUES ('delivery_abandonment', ?, ?)",
        )
        .bind(&abandonment.delivery_id)
        .bind(&payload)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO outbox(id, kind, payload) VALUES (?, 'delivery_abandonment', ?)")
            .bind(format!("abandonment:{}", abandonment.operation_id))
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('host_operation', ?, ?)")
            .bind(operation_key)
            .bind(serde_json::to_string(&serde_json::json!({
                "digest": digest,
                "result": response,
            }))?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.notify();
        Ok(response)
    }
}

#[cfg(test)]
#[path = "delivery_abandonment_tests.rs"]
mod tests;
