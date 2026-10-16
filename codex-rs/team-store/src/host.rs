use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamControlState;
use codex_app_server_protocol::TeamDelivery;
use codex_app_server_protocol::TeamEnrollment;
use codex_app_server_protocol::TeamReceipt;
use codex_app_server_protocol::TeamReceiptState;

use crate::TeamStore;

impl TeamStore {
    pub async fn control(&self) -> Result<Option<TeamControlState>> {
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM control WHERE singleton = 1")
                .fetch_optional(&self.pool)
                .await?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    /// Commit monotonic control before acknowledging or releasing any host work.
    pub async fn apply_control(&self, control: &TeamControlState) -> Result<TeamControlState> {
        self.commit_control(control, None)
            .await
            .map(|(current, _)| current)
    }

    /// Commit an operator's logical control operation, result and control state
    /// atomically. A retry reads its original result instead of issuing a new revision.
    pub async fn apply_control_once(
        &self,
        control: &TeamControlState,
        operation_id: &str,
        digest: &str,
        result: &serde_json::Value,
    ) -> Result<bool> {
        self.commit_control(control, Some((operation_id, digest, result)))
            .await
            .map(|(_, inserted)| inserted)
    }

    async fn commit_control(
        &self,
        control: &TeamControlState,
        operation: Option<(&str, &str, &serde_json::Value)>,
    ) -> Result<(TeamControlState, bool)> {
        ensure!(
            control.revision >= 0,
            "control revision must not be negative"
        );
        ensure!(
            !control.coordinator_id.is_empty(),
            "coordinator identity is required"
        );
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some((id, digest, _)) = operation {
            let previous: Option<String> = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'operator_operation' AND id = ?",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(previous) = previous {
                ensure!(
                    previous == digest,
                    "operation identity has conflicting content"
                );
                return Ok((control.clone(), false));
            }
        }
        sqlx::query("INSERT INTO control(singleton, coordinator_id, revision, payload) VALUES (1, ?, ?, ?) ON CONFLICT(singleton) DO UPDATE SET revision = excluded.revision, payload = excluded.payload WHERE control.coordinator_id = excluded.coordinator_id AND control.revision < excluded.revision")
            .bind(&control.coordinator_id).bind(control.revision).bind(serde_json::to_string(control)?)
            .execute(&mut *tx).await?;
        let payload: String = sqlx::query_scalar("SELECT payload FROM control WHERE singleton = 1")
            .fetch_one(&mut *tx)
            .await?;
        let current: TeamControlState = serde_json::from_str(&payload)?;
        ensure!(
            current.coordinator_id == control.coordinator_id,
            "host belongs to a different coordinator"
        );
        ensure!(
            current.revision != control.revision || current == *control,
            "control revision has conflicting content"
        );
        // Keep control history in the same transaction. Offline hosts must see
        // a stop followed by a resume, even if both happened while disconnected.
        sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('control_history', ?, ?) ON CONFLICT(namespace, id) DO NOTHING")
            .bind(format!("{:020}", current.revision)).bind(&payload).execute(&mut *tx).await?;
        if current.mode == codex_app_server_protocol::TeamExecutionMode::Stopped {
            let roots: Vec<String> =
                sqlx::query_scalar("SELECT thread_id FROM enrollment ORDER BY thread_id")
                    .fetch_all(&mut *tx)
                    .await?;
            sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('control_scope', ?, ?) ON CONFLICT(namespace, id) DO NOTHING")
                .bind(format!("{:020}", current.revision)).bind(serde_json::to_string(&roots)?)
                .execute(&mut *tx).await?;
        }
        if let Some((id, digest, result)) = operation {
            sqlx::query(
                "INSERT INTO record(namespace, id, payload) VALUES ('operator_operation', ?, ?)",
            )
            .bind(id)
            .bind(digest)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO record(namespace, id, payload) VALUES ('operator_result', ?, ?)",
            )
            .bind(id)
            .bind(serde_json::to_string(result)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.notify();
        Ok((current, true))
    }

    pub async fn enroll(
        &self,
        enrollment: &TeamEnrollment,
        credential: &str,
    ) -> Result<TeamEnrollment> {
        self.enroll_operation(enrollment, credential, None).await
    }

    pub async fn host_operation_result<T: serde::de::DeserializeOwned>(
        &self,
        id: &str,
        digest: &str,
    ) -> Result<Option<T>> {
        let Some(previous) = self.get::<serde_json::Value>("host_operation", id).await? else {
            return Ok(None);
        };
        ensure!(
            previous["digest"].as_str() == Some(digest),
            "operation identity has conflicting content"
        );
        Ok(Some(serde_json::from_value(previous["result"].clone())?))
    }

    pub async fn enroll_operation(
        &self,
        enrollment: &TeamEnrollment,
        credential: &str,
        operation: Option<(&str, &str)>,
    ) -> Result<TeamEnrollment> {
        // Re-enrollment can rename an alias, but cannot erase a manual hold,
        // reset a pending continuation, or rotate an agent's credential.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some((id, digest)) = operation {
            let previous: Option<String> = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'host_operation' AND id = ?",
            )
            .bind(id)
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
        }
        sqlx::query("INSERT INTO enrollment(thread_id, payload, credential) VALUES (?, ?, ?) ON CONFLICT(thread_id) DO NOTHING")
            .bind(&enrollment.address.thread_id).bind(serde_json::to_string(enrollment)?).bind(credential)
            .execute(&mut *tx).await?;
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM enrollment WHERE thread_id = ?")
                .bind(&enrollment.address.thread_id)
                .fetch_one(&mut *tx)
                .await?;
        let mut current: TeamEnrollment = serde_json::from_str(&payload)?;
        ensure!(
            current.address == enrollment.address,
            "enrolled thread address cannot be reassigned"
        );
        current.alias.clone_from(&enrollment.alias);
        sqlx::query("UPDATE enrollment SET payload = ? WHERE thread_id = ?")
            .bind(serde_json::to_string(&current)?)
            .bind(&current.address.thread_id)
            .execute(&mut *tx)
            .await?;
        let control: Option<String> =
            sqlx::query_scalar("SELECT payload FROM control WHERE singleton = 1")
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(control) = control {
            let control: TeamControlState = serde_json::from_str(&control)?;
            if control.mode == codex_app_server_protocol::TeamExecutionMode::Stopped {
                let revision = format!("{:020}", control.revision);
                let scope: Option<String> = sqlx::query_scalar(
                    "SELECT payload FROM record WHERE namespace = 'control_scope' AND id = ?",
                )
                .bind(&revision)
                .fetch_optional(&mut *tx)
                .await?;
                let mut roots: Vec<String> = scope
                    .map(|scope| serde_json::from_str(&scope))
                    .transpose()?
                    .unwrap_or_default();
                if !roots.contains(&current.address.thread_id) {
                    roots.push(current.address.thread_id.clone());
                    roots.sort();
                    sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('control_scope', ?, ?) ON CONFLICT(namespace, id) DO UPDATE SET payload = excluded.payload")
                        .bind(&revision).bind(serde_json::to_string(&roots)?).execute(&mut *tx).await?;
                    sqlx::query(
                        "DELETE FROM record WHERE namespace = 'control_cleanup_done' AND id = ?",
                    )
                    .bind(&revision)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        if let Some((id, digest)) = operation {
            sqlx::query(
                "INSERT INTO record(namespace, id, payload) VALUES ('host_operation', ?, ?)",
            )
            .bind(id)
            .bind(serde_json::to_string(
                &serde_json::json!({"digest": digest, "result": current}),
            )?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.notify();
        Ok(current)
    }

    /// Revoke enrollment and credentials together with explicit rejection of
    /// queued input. Attempt evidence, manual holds and external jobs survive.
    pub async fn unenroll(
        &self,
        root: &str,
        members: &[String],
        operation: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some((id, digest)) = operation {
            let previous: Option<String> = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'host_operation' AND id = ?",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(previous) = previous {
                let previous: serde_json::Value = serde_json::from_str(&previous)?;
                ensure!(
                    previous["digest"].as_str() == Some(digest),
                    "operation identity has conflicting content"
                );
                return Ok(());
            }
        }
        for member in members {
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT id, receipt FROM delivery WHERE thread_id = ?")
                    .bind(member)
                    .fetch_all(&mut *tx)
                    .await?;
            for (id, payload) in rows {
                let mut receipt: TeamReceipt = serde_json::from_str(&payload)?;
                if receipt.state != TeamReceiptState::Pending {
                    continue;
                }
                receipt.state = TeamReceiptState::Rejected;
                receipt.detail =
                    Some("Recipient left the team before this input was dispatched.".into());
                let payload = serde_json::to_string(&receipt)?;
                sqlx::query("UPDATE delivery SET receipt = ? WHERE id = ?")
                    .bind(&payload)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("INSERT INTO outbox(id, kind, payload) VALUES ('receipt:' || ? || ':' || (SELECT COALESCE(MAX(sequence), 0) + 1 FROM outbox), 'receipt', ?)")
                    .bind(&id).bind(&payload).execute(&mut *tx).await?;
            }
            sqlx::query("DELETE FROM record WHERE namespace IN ('member_credential', 'model_selection_request', 'model_local_intent', 'model_recovery_error', 'leaving') AND id = ?")
                .bind(member).execute(&mut *tx).await?;
        }
        let enrollment: Option<String> =
            sqlx::query_scalar("SELECT payload FROM enrollment WHERE thread_id = ?")
                .bind(root)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(enrollment) = enrollment
            && serde_json::from_str::<TeamEnrollment>(&enrollment)?.manually_held
        {
            sqlx::query("INSERT INTO record(namespace, id, payload) VALUES ('manual_hold', ?, 'true') ON CONFLICT(namespace, id) DO UPDATE SET payload = 'true'")
                .bind(root).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM enrollment WHERE thread_id = ?")
            .bind(root)
            .execute(&mut *tx)
            .await?;
        if let Some((id, digest)) = operation {
            sqlx::query(
                "INSERT INTO record(namespace, id, payload) VALUES ('host_operation', ?, ?)",
            )
            .bind(id)
            .bind(serde_json::to_string(
                &serde_json::json!({"digest": digest, "result": {"threadId": root}}),
            )?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.notify();
        Ok(())
    }

    pub async fn enrollment(&self, thread_id: &str) -> Result<Option<TeamEnrollment>> {
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM enrollment WHERE thread_id = ?")
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    pub async fn enrollments(&self, after: &str, limit: u32) -> Result<Vec<TeamEnrollment>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT payload FROM enrollment WHERE thread_id > ? ORDER BY thread_id LIMIT ?",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(Into::into))
            .collect()
    }

    pub async fn update_enrollment(&self, enrollment: &TeamEnrollment) -> Result<()> {
        let result = sqlx::query("UPDATE enrollment SET payload = ? WHERE thread_id = ?")
            .bind(serde_json::to_string(enrollment)?)
            .bind(&enrollment.address.thread_id)
            .execute(&self.pool)
            .await?;
        ensure!(result.rows_affected() == 1, "thread is not enrolled");
        self.notify();
        Ok(())
    }

    pub async fn credential(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT credential FROM enrollment WHERE thread_id = ?")
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Repeated deliveries return their original receipt. Conflicting reuse of
    /// an identity is rejected rather than becoming a second agent action.
    pub async fn receive(&self, delivery: &TeamDelivery) -> Result<TeamReceipt> {
        let initial = TeamReceipt {
            delivery_id: delivery.id.clone(),
            thread_id: delivery.thread_id.clone(),
            state: TeamReceiptState::Pending,
            turn_id: None,
            detail: None,
        };
        let payload = serde_json::to_string(delivery)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO delivery(id, thread_id, payload, receipt) VALUES (?, ?, ?, ?) ON CONFLICT(id) DO NOTHING")
            .bind(&delivery.id).bind(&delivery.thread_id).bind(&payload).bind(serde_json::to_string(&initial)?)
            .execute(&mut *tx).await?;
        let (stored_payload, receipt): (String, String) =
            sqlx::query_as("SELECT payload, receipt FROM delivery WHERE id = ?")
                .bind(&delivery.id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            stored_payload == payload,
            "delivery identity has conflicting content"
        );
        let receipt = serde_json::from_str(&receipt)?;
        tx.commit().await?;
        self.notify();
        Ok(receipt)
    }

    pub async fn deliveries(&self, thread_id: &str) -> Result<Vec<(TeamDelivery, TeamReceipt)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT payload, receipt FROM delivery WHERE thread_id = ? ORDER BY sequence",
        )
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(delivery, receipt)| {
                Ok((
                    serde_json::from_str(&delivery)?,
                    serde_json::from_str(&receipt)?,
                ))
            })
            .collect()
    }

    pub async fn update_receipt(&self, receipt: &TeamReceipt) -> Result<()> {
        let payload = serde_json::to_string(receipt)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<String> =
            sqlx::query_scalar("SELECT receipt FROM delivery WHERE id = ? AND thread_id = ?")
                .bind(&receipt.delivery_id)
                .bind(&receipt.thread_id)
                .fetch_optional(&mut *tx)
                .await?;
        if previous.as_deref() == Some(&payload) {
            return Ok(());
        }
        let result = sqlx::query("UPDATE delivery SET receipt = ? WHERE id = ? AND thread_id = ?")
            .bind(&payload)
            .bind(&receipt.delivery_id)
            .bind(&receipt.thread_id)
            .execute(&mut *tx)
            .await?;
        ensure!(result.rows_affected() == 1, "delivery does not exist");
        // Receipt and publication enter the ledger together. The coordinator
        // can replay this outbox after a disconnect without replaying the turn.
        // Every transition gets a fresh sequence. Updating an older row would
        // let an in-flight acknowledgement erase a newer receipt transition.
        sqlx::query("INSERT INTO outbox(id, kind, payload) VALUES ('receipt:' || ? || ':' || (SELECT COALESCE(MAX(sequence), 0) + 1 FROM outbox), 'receipt', ?)")
            .bind(&receipt.delivery_id).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        self.notify();
        Ok(())
    }
}
