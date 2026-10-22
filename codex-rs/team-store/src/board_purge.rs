//! Permanent Board cleanup commits selection, erasure and retry evidence together.

use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde_json::json;
use sqlx::Sqlite;
use sqlx::Transaction;

use crate::BoardClock;
use crate::TeamStore;
use crate::board_purge_content::purge_coordinator_content;
use crate::board_purge_content::put_record;

pub struct BoardPurgeCommit {
    pub scope: TeamBoardPurgeScope,
    pub expected_revision: i64,
    pub operation_id: String,
    pub digest: String,
    pub workers: Vec<TeamBoardPurgeWorker>,
}

impl TeamStore {
    pub async fn preview_board_purge(
        &self,
        scope: &TeamBoardPurgeScope,
    ) -> Result<TeamBoardPurgePreview> {
        let mut tx = self.pool.begin().await?;
        let (clock, conversations, messages) = selection(&mut tx, scope).await?;
        Ok(TeamBoardPurgePreview {
            scope: scope.clone(),
            revision: clock.revision,
            conversation_count: conversations.len().try_into()?,
            message_count: messages.len().try_into()?,
        })
    }

    pub async fn purge_board(&self, commit: BoardPurgeCommit) -> Result<TeamBoardPurgeOperation> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let global: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM record WHERE namespace = 'operator_operation' AND id = ?",
        )
        .bind(&commit.operation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(global) = &global {
            ensure!(
                *global == commit.digest,
                "operation identity has conflicting content"
            );
            let rejection: Option<String> = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'operator_rejection' AND id = ?",
            )
            .bind(&commit.operation_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(rejection) = rejection {
                let rejection: serde_json::Value = serde_json::from_str(&rejection)?;
                anyhow::bail!(
                    "{}",
                    rejection["reason"]
                        .as_str()
                        .unwrap_or("Board purge was rejected")
                );
            }
        }
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT payload FROM record WHERE namespace = 'board_purge_digest' AND id = ?",
        )
        .bind(&commit.operation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            ensure!(
                serde_json::from_str::<String>(&previous)? == commit.digest,
                "operation identity has conflicting content"
            );
            let payload: String = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'board_purge_operation' AND id = ?",
            )
            .bind(&commit.operation_id)
            .fetch_one(&mut *tx)
            .await?;
            return Ok(serde_json::from_str(&payload)?);
        }
        ensure!(
            global.is_none(),
            "operation identity belongs to another operation"
        );

        let (mut clock, conversations, messages) = selection(&mut tx, &commit.scope).await?;
        if clock.revision != commit.expected_revision {
            let reason = "Board changed since preview; preview the scope again";
            put_record(
                &mut tx,
                "operator_operation",
                &commit.operation_id,
                &serde_json::from_str(&commit.digest)?,
            )
            .await?;
            put_record(
                &mut tx,
                "operator_rejection",
                &commit.operation_id,
                &json!({"operationId":commit.operation_id,"reason":reason}),
            )
            .await?;
            tx.commit().await?;
            self.notify();
            anyhow::bail!(reason);
        }
        clock.revision = clock
            .revision
            .checked_add(1)
            .context("Board revision exhausted")?;
        let mut journal = TeamBoardPurgeJournal {
            operation_id: commit.operation_id.clone(),
            revision: clock.revision,
            conversation_ids: conversations.into_iter().collect(),
            message_ids: messages.into_iter().collect(),
        };
        let operation = TeamBoardPurgeOperation {
            operation_id: commit.operation_id.clone(),
            scope: commit.scope,
            revision: clock.revision,
            conversation_count: journal.conversation_ids.len().try_into()?,
            message_count: journal.message_ids.len().try_into()?,
            workers: commit
                .workers
                .into_iter()
                .map(|worker| TeamBoardPurgeWorker {
                    acknowledged: false,
                    ..worker
                })
                .collect(),
        };
        purge_coordinator_content(&mut tx, &mut journal).await?;
        for (namespace, id, value) in [
            ("board_clock", "current", serde_json::to_value(clock)?),
            (
                "board_purge_journal",
                commit.operation_id.as_str(),
                serde_json::to_value(&journal)?,
            ),
            (
                "board_purge_digest",
                commit.operation_id.as_str(),
                json!(commit.digest),
            ),
            (
                "board_purge_operation",
                commit.operation_id.as_str(),
                serde_json::to_value(&operation)?,
            ),
            (
                "operator_operation",
                commit.operation_id.as_str(),
                serde_json::from_str(&commit.digest)?,
            ),
            (
                "operator_result",
                commit.operation_id.as_str(),
                json!({"type":"operation","operation":{"type":"boardPurge","operation":operation}}),
            ),
        ] {
            put_record(&mut tx, namespace, id, &value).await?;
        }
        tx.commit().await?;
        self.notify();
        Ok(operation)
    }

    pub async fn board_purge_operation(&self, id: &str) -> Result<Option<TeamBoardPurgeOperation>> {
        self.get("board_purge_operation", id).await
    }

    pub async fn board_purge_journal(&self) -> Result<Vec<TeamBoardPurgeJournal>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT payload FROM record WHERE namespace = 'board_purge_journal' ORDER BY json_extract(payload, '$.revision')",
        ).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(Into::into))
            .collect()
    }

    pub async fn acknowledge_board_purges(
        &self,
        installation_id: &str,
        ids: &[String],
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut changed = false;
        for id in ids {
            let payload: String = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'board_purge_operation' AND id = ?",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let mut operation: TeamBoardPurgeOperation = serde_json::from_str(&payload)?;
            let mut operation_changed = false;
            for worker in &mut operation.workers {
                if worker.installation_id == installation_id && !worker.acknowledged {
                    worker.acknowledged = true;
                    operation_changed = true;
                }
            }
            if operation_changed {
                put_record(
                    &mut tx,
                    "board_purge_operation",
                    id,
                    &serde_json::to_value(&operation)?,
                )
                .await?;
                put_record(&mut tx, "operator_result", id, &json!({"type":"operation","operation":{"type":"boardPurge","operation":operation}})).await?;
                changed = true;
            }
        }
        tx.commit().await?;
        if changed {
            self.notify();
        }
        Ok(changed)
    }

    pub async fn board_message_removed(&self, id: &str) -> Result<bool> {
        Ok(self
            .get::<String>("board_removed_message", id)
            .await?
            .is_some())
    }

    pub async fn board_conversation_removed(&self, id: &str) -> Result<bool> {
        Ok(self
            .get::<String>("board_removed_conversation", id)
            .await?
            .is_some())
    }
}

async fn selection(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &TeamBoardPurgeScope,
) -> Result<(BoardClock, BTreeSet<String>, BTreeSet<String>)> {
    let clock: Option<String> = sqlx::query_scalar(
        "SELECT payload FROM record WHERE namespace = 'board_clock' AND id = 'current'",
    )
    .fetch_optional(&mut **tx)
    .await?;
    let clock = clock
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default();
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM record WHERE namespace = 'board_entry'")
            .fetch_all(&mut **tx)
            .await?;
    let entries = rows
        .into_iter()
        .map(|row| serde_json::from_str::<TeamBoardEntry>(&row))
        .collect::<Result<Vec<_>, _>>()?;
    let conversations = entries
        .iter()
        .filter(|entry| match scope {
            TeamBoardPurgeScope::Conversation { conversation_id } => {
                entry.conversation_id == *conversation_id
            }
            TeamBoardPurgeScope::Topic { topic } => entry.message.topic.as_ref() == Some(topic),
            TeamBoardPurgeScope::All => true,
        })
        .map(|entry| entry.conversation_id.clone())
        .collect::<BTreeSet<_>>();
    let messages = entries
        .into_iter()
        .filter(|entry| conversations.contains(&entry.conversation_id))
        .map(|entry| entry.message.id)
        .collect();
    Ok((clock, conversations, messages))
}

#[cfg(test)]
#[path = "board_purge_tests.rs"]
mod tests;
