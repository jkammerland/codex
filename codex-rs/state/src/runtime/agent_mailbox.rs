use std::sync::Arc;

use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteRow;

use crate::AgentMailAcceptance;
use crate::AgentMailEnvelope;
use crate::AgentMailRecord;
use crate::AgentMailboxSummary;
use crate::AgentTaskFilter;
use crate::AgentTaskRecord;
use crate::AgentTaskSnapshot;
use crate::AgentTaskStatus;
use crate::SqlitePoolWriteExt;
use crate::validate_agent_task_completion;
use crate::validate_agent_task_identity;
use crate::validate_agent_task_request;
use crate::validate_agent_task_update;

/// Durable mail and tasks sharing the queue database's transaction boundary.
#[derive(Clone)]
pub struct SqliteAgentMailboxStore {
    pool: Arc<SqlitePool>,
}

impl SqliteAgentMailboxStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    pub async fn accept(&self, envelope: AgentMailEnvelope) -> anyhow::Result<AgentMailAcceptance> {
        let mut tx = self.pool.begin_immediate().await?;
        let accepted = accept_in_tx(&mut tx, &envelope).await?;
        tx.commit().await?;
        Ok(accepted)
    }

    /// Recorded mail remains pending until an actual model request includes it.
    pub async fn pending(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
    ) -> anyhow::Result<Vec<AgentMailRecord>> {
        sqlx::query(
            "SELECT * FROM agent_mail WHERE root_id = ? AND recipient_id = ?
             AND submitted_turn_id IS NULL ORDER BY sequence",
        )
        .bind(root_id.to_string())
        .bind(recipient_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await?
        .iter()
        .map(mail_from_row)
        .collect()
    }

    /// Select accepted IDs from actual model input, including previously submitted mail.
    /// Results are distinct and retain acceptance order.
    pub async fn accepted_ids(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
    ) -> anyhow::Result<Vec<String>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT message_id FROM agent_mail WHERE root_id = ? AND recipient_id = ?
             AND message_id IN (SELECT value FROM json_each(?)) ORDER BY sequence",
        )
        .bind(root_id.to_string())
        .bind(recipient_id.to_string())
        .bind(serde_json::to_string(&message_ids)?)
        .fetch_all(self.pool.as_ref())
        .await?)
    }

    /// Bind assignments selected for a request to its turn without acknowledging
    /// server acceptance or consuming their pending mail.
    pub async fn mark_attempted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> anyhow::Result<()> {
        self.mark_receipts(
            root_id,
            recipient_id,
            message_ids,
            turn_id,
            Receipt::Attempted,
        )
        .await
    }

    pub async fn mark_recorded(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> anyhow::Result<()> {
        self.mark_receipts(
            root_id,
            recipient_id,
            message_ids,
            turn_id,
            Receipt::Recorded,
        )
        .await
    }

    pub async fn mark_submitted(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
    ) -> anyhow::Result<()> {
        self.mark_receipts(
            root_id,
            recipient_id,
            message_ids,
            turn_id,
            Receipt::Submitted,
        )
        .await
    }

    async fn mark_receipts(
        &self,
        root_id: ThreadId,
        recipient_id: ThreadId,
        message_ids: Vec<String>,
        turn_id: String,
        receipt: Receipt,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        for message_id in message_ids {
            let row = sqlx::query(
                "SELECT * FROM agent_mail WHERE message_id = ? AND root_id = ? AND recipient_id = ?",
            )
            .bind(&message_id)
            .bind(root_id.to_string())
            .bind(recipient_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| invalid("receipt references mail outside this recipient mailbox"))?;
            let record = mail_from_row(&row)?;
            let query = match receipt {
                Receipt::Attempted => None,
                Receipt::Recorded if record.recorded_turn_id.is_some() => continue,
                Receipt::Recorded => {
                    Some("UPDATE agent_mail SET recorded_turn_id = ? WHERE message_id = ?")
                }
                Receipt::Submitted if record.recorded_turn_id.is_none() => {
                    return Err(invalid("mail must be recorded before submission"));
                }
                Receipt::Submitted if record.submitted_turn_id.is_some() => None,
                Receipt::Submitted => {
                    Some("UPDATE agent_mail SET submitted_turn_id = ? WHERE message_id = ?")
                }
            };
            if let Some(query) = query {
                sqlx::query(query)
                    .bind(&turn_id)
                    .bind(&message_id)
                    .execute(&mut *tx)
                    .await?;
                bump_revision(&mut tx, root_id).await?;
            }

            // Preserve the first receipt while rebinding held work when a later
            // request actually includes the assignment again.
            if !matches!(receipt, Receipt::Recorded)
                && let Some(task_id) = &record.envelope.task_id
                && let Some(mut task) = load_task(&mut tx, task_id).await?
                && task.root_id == root_id
                && task.target_id == recipient_id
                && !task.status.is_terminal()
            {
                task.status = match receipt {
                    Receipt::Submitted => AgentTaskStatus::Running,
                    Receipt::Attempted | Receipt::Recorded
                        if task.turn_id.as_ref() == Some(&turn_id)
                            && task.status == AgentTaskStatus::Running =>
                    {
                        AgentTaskStatus::Running
                    }
                    Receipt::Attempted | Receipt::Recorded => AgentTaskStatus::Queued,
                };
                task.turn_id = Some(turn_id.clone());
                put_task_in_tx(&mut tx, &task).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn put_task(&self, task: AgentTaskRecord) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        put_task_in_tx(&mut tx, &task).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Accept a task and its request together, without leaving orphaned queued work.
    pub async fn accept_task(
        &self,
        task: AgentTaskRecord,
        request: AgentMailEnvelope,
    ) -> anyhow::Result<AgentMailAcceptance> {
        validate_agent_task_request(&task, &request)?;
        let mut tx = self.pool.begin_immediate().await?;
        let accepted = accept_in_tx(&mut tx, &request).await?;
        // A retried original request must not reset an already running task.
        if accepted.inserted {
            if load_task(&mut tx, &task.task_id).await?.is_some() {
                return Err(invalid("task already has an accepted initial request"));
            }
            put_task_in_tx(&mut tx, &task).await?;
        } else {
            let previous = load_task(&mut tx, &task.task_id)
                .await?
                .ok_or_else(|| invalid("accepted task request has no task record"))?;
            validate_agent_task_identity(&previous, &task)?;
        }
        tx.commit().await?;
        Ok(accepted)
    }

    /// Commit terminal state and owner-addressed completion mail atomically.
    pub async fn settle_task(
        &self,
        task: AgentTaskRecord,
        completion: AgentMailEnvelope,
    ) -> anyhow::Result<AgentMailAcceptance> {
        validate_agent_task_completion(&task, &completion)?;
        let mut tx = self.pool.begin_immediate().await?;
        put_task_in_tx(&mut tx, &task).await?;
        let accepted = accept_in_tx(&mut tx, &completion).await?;
        tx.commit().await?;
        Ok(accepted)
    }

    pub async fn tasks(
        &self,
        root_id: ThreadId,
        filter: AgentTaskFilter,
    ) -> anyhow::Result<AgentTaskSnapshot> {
        // Read the revision and rows from the same snapshot.
        let mut tx = self.pool.begin().await?;
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM agent_mailbox_roots WHERE root_id = ?",
        )
        .bind(root_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_default();
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT record_json FROM agent_tasks WHERE root_id = ? ORDER BY task_id",
        )
        .bind(root_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let tasks = rows
            .iter()
            .map(|json| serde_json::from_str::<AgentTaskRecord>(json))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|task| filter.matches(task))
            .collect();
        tx.commit().await?;
        Ok(AgentTaskSnapshot { revision, tasks })
    }

    pub async fn summary(&self, root_id: ThreadId) -> anyhow::Result<AgentMailboxSummary> {
        let (revision, pending_mail, unsettled_tasks) = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT revision,
             (SELECT COUNT(*) FROM agent_mail WHERE root_id = roots.root_id AND submitted_turn_id IS NULL),
             (SELECT COUNT(*) FROM agent_tasks WHERE root_id = roots.root_id AND terminal = 0)
             FROM agent_mailbox_roots roots WHERE root_id = ?",
        )
        .bind(root_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?
        .unwrap_or_default();
        Ok(AgentMailboxSummary {
            revision,
            pending_mail,
            unsettled_tasks,
        })
    }

    /// Root lifecycle removal cascades to its mail and task receipts.
    pub async fn remove_root(&self, root_id: ThreadId) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_immediate().await?;
        sqlx::query("DELETE FROM agent_mailbox_roots WHERE root_id = ?")
            .bind(root_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Receipt {
    Attempted,
    Recorded,
    Submitted,
}

fn invalid(message: &str) -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into()
}

fn mail_from_row(row: &SqliteRow) -> anyhow::Result<AgentMailRecord> {
    Ok(AgentMailRecord {
        envelope: serde_json::from_str(row.try_get("envelope_json")?)?,
        sequence: row.try_get("sequence")?,
        recorded_turn_id: row.try_get("recorded_turn_id")?,
        submitted_turn_id: row.try_get("submitted_turn_id")?,
    })
}

async fn bump_revision(connection: &mut SqliteConnection, root_id: ThreadId) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO agent_mailbox_roots(root_id, revision) VALUES (?, 1)
         ON CONFLICT(root_id) DO UPDATE SET revision = revision + 1",
    )
    .bind(root_id.to_string())
    .execute(connection)
    .await?;
    Ok(())
}

async fn accept_in_tx(
    connection: &mut SqliteConnection,
    envelope: &AgentMailEnvelope,
) -> anyhow::Result<AgentMailAcceptance> {
    if let Some(row) = sqlx::query("SELECT * FROM agent_mail WHERE message_id = ?")
        .bind(&envelope.message_id)
        .fetch_optional(&mut *connection)
        .await?
    {
        let record = mail_from_row(&row)?;
        if record.envelope != *envelope {
            return Err(invalid(
                "message ID already accepted with a different envelope",
            ));
        }
        return Ok(AgentMailAcceptance {
            record,
            inserted: false,
        });
    }

    bump_revision(connection, envelope.root_id).await?;
    let row = sqlx::query(
        "INSERT INTO agent_mail(message_id, root_id, recipient_id, envelope_json)
         VALUES (?, ?, ?, ?) RETURNING *",
    )
    .bind(&envelope.message_id)
    .bind(envelope.root_id.to_string())
    .bind(envelope.recipient_id.to_string())
    .bind(serde_json::to_string(envelope)?)
    .fetch_one(connection)
    .await?;
    Ok(AgentMailAcceptance {
        record: mail_from_row(&row)?,
        inserted: true,
    })
}

async fn load_task(
    connection: &mut SqliteConnection,
    task_id: &str,
) -> anyhow::Result<Option<AgentTaskRecord>> {
    sqlx::query_scalar::<_, String>("SELECT record_json FROM agent_tasks WHERE task_id = ?")
        .bind(task_id)
        .fetch_optional(connection)
        .await?
        .map(|json| serde_json::from_str(&json).map_err(Into::into))
        .transpose()
}

async fn put_task_in_tx(
    connection: &mut SqliteConnection,
    task: &AgentTaskRecord,
) -> anyhow::Result<()> {
    if let Some(previous) = load_task(connection, &task.task_id).await? {
        validate_agent_task_update(&previous, task)?;
        if previous == *task {
            return Ok(());
        }
    }
    bump_revision(connection, task.root_id).await?;
    sqlx::query(
        "INSERT INTO agent_tasks(task_id, root_id, record_json, terminal) VALUES (?, ?, ?, ?)
         ON CONFLICT(task_id) DO UPDATE SET record_json = excluded.record_json, terminal = excluded.terminal",
    )
    .bind(&task.task_id)
    .bind(task.root_id.to_string())
    .bind(serde_json::to_string(task)?)
    .bind(task.status.is_terminal())
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "agent_mailbox_tests.rs"]
mod tests;
