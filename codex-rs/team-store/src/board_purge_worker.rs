//! Worker purge acknowledgements follow erasure of all managed content copies.

use std::collections::BTreeSet;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde_json::Value;
use serde_json::json;

use crate::TeamStore;
use crate::board_purge_content::expand_removed_messages;
use crate::board_purge_content::mark_removed;
use crate::board_purge_content::matches_removed;
use crate::board_purge_content::put_record;

impl TeamStore {
    pub async fn apply_board_purges(
        &self,
        journals: &[TeamBoardPurgeJournal],
    ) -> Result<Vec<String>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut acknowledgements = Vec::new();
        let mut changed = false;
        for journal in journals {
            let previous: Option<String> = sqlx::query_scalar(
                "SELECT payload FROM record WHERE namespace = 'board_purge_applied' AND id = ?",
            )
            .bind(&journal.operation_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(previous) = previous {
                ensure!(
                    serde_json::from_str::<TeamBoardPurgeJournal>(&previous)? == *journal,
                    "Board purge identity has conflicting content"
                );
                acknowledgements.push(journal.operation_id.clone());
                continue;
            }
            mark_removed(&mut tx, journal).await?;
            let outbox: Vec<(String, String)> =
                sqlx::query_as("SELECT id,payload FROM outbox WHERE kind IN ('message','board')")
                    .fetch_all(&mut *tx)
                    .await?;
            let outbox = outbox
                .into_iter()
                .map(|(id, payload)| Ok((id, serde_json::from_str::<Value>(&payload)?)))
                .collect::<Result<Vec<_>>>()?;
            let deliveries: Vec<(String, String, String)> =
                sqlx::query_as("SELECT id,payload,receipt FROM delivery")
                    .fetch_all(&mut *tx)
                    .await?;
            let mut candidates = outbox.clone();
            for (_, payload, _) in &deliveries {
                let value: Value = serde_json::from_str(payload)?;
                if let Some(id) = value.pointer("/message/id").and_then(Value::as_str) {
                    candidates.push((id.to_owned(), value));
                }
            }
            let mut removed = journal
                .message_ids
                .iter()
                .chain(&journal.conversation_ids)
                .map(|id| (id.clone(), journal.operation_id.clone()))
                .collect();
            expand_removed_messages(&mut removed, &candidates);
            for (id, operation) in &removed {
                put_record(&mut tx, "board_removed_message", id, &json!(operation)).await?;
            }
            let messages = removed.into_keys().collect::<BTreeSet<_>>();
            let conversations = journal
                .conversation_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT namespace,id,payload FROM record WHERE namespace IN ('board_cache','message_status_cache') OR namespace LIKE 'board:%'",
            ).fetch_all(&mut *tx).await?;
            for (namespace, id, payload) in rows {
                if namespace == "board_cache" {
                    let mut snapshot: TeamBoardSnapshot = serde_json::from_str(&payload)?;
                    remove_from_snapshot(&mut snapshot, &messages, &conversations);
                    snapshot.revision = snapshot.revision.max(journal.revision);
                    put_record(&mut tx, &namespace, &id, &serde_json::to_value(snapshot)?).await?;
                } else if namespace == "message_status_cache" {
                    let mut snapshot: TeamMessageStatusSnapshot = serde_json::from_str(&payload)?;
                    clear_status_details(&mut snapshot, &messages);
                    put_record(&mut tx, &namespace, &id, &serde_json::to_value(snapshot)?).await?;
                } else if messages.contains(id.as_str()) {
                    sqlx::query("DELETE FROM record WHERE namespace = ? AND id = ?")
                        .bind(&namespace)
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                }
            }

            // Even acknowledged source outboxes keep full request bodies. Keep
            // their IDs but replace the payload and remove them from delivery.
            for (id, value) in outbox {
                if matches_removed(&id, &value, &messages, &conversations) {
                    sqlx::query("UPDATE outbox SET kind = 'removed', payload = ?, acknowledged = 1 WHERE id = ?")
                        .bind(serde_json::to_string(&json!({"removed":true,"messageId":id}))?)
                        .bind(&id).execute(&mut *tx).await?;
                }
            }

            for (id, payload, receipt) in deliveries {
                let mut delivery: TeamDelivery = serde_json::from_str(&payload)?;
                if !messages.contains(delivery.message.id.as_str()) {
                    continue;
                }
                delivery.message.body.clear();
                let mut receipt: TeamReceipt = serde_json::from_str(&receipt)?;
                // A dispatch already admitted under the host lock may finish.
                // Only a proven pending delivery can become suppressed.
                if receipt.state == TeamReceiptState::Pending {
                    receipt.state = TeamReceiptState::Rejected;
                    receipt.detail = Some("Removed before delivery.".into());
                } else {
                    receipt.detail = None;
                }
                sqlx::query("UPDATE delivery SET payload = ?, receipt = ? WHERE id = ?")
                    .bind(serde_json::to_string(&delivery)?)
                    .bind(serde_json::to_string(&receipt)?)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                // The original receipt may have committed remotely with its ACK
                // lost. Retire its identity instead of replaying changed content.
                sqlx::query("UPDATE outbox SET kind = 'removed', payload = json_object('removed', json('true'), 'deliveryId', ?), acknowledged = 1 WHERE kind = 'receipt' AND json_extract(payload, '$.deliveryId') = ?")
                    .bind(&id)
                    .bind(&id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO outbox(id,kind,payload) VALUES ('purge-receipt:' || ? || ':' || ?, 'receipt', ?) ON CONFLICT(id) DO NOTHING")
                    .bind(&journal.operation_id).bind(&id).bind(serde_json::to_string(&receipt)?)
                    .execute(&mut *tx).await?;
            }
            put_record(
                &mut tx,
                "board_purge_applied",
                &journal.operation_id,
                &serde_json::to_value(journal)?,
            )
            .await?;
            acknowledgements.push(journal.operation_id.clone());
            changed = true;
        }
        tx.commit().await?;
        if changed {
            self.notify();
        }
        Ok(acknowledgements)
    }

    /// Old syncs may arrive after purge. Never restore removed cache entries.
    pub async fn filter_removed_board_snapshot(
        &self,
        snapshot: &mut TeamBoardSnapshot,
    ) -> Result<()> {
        let messages = self
            .list::<String>("board_removed_message", "", u32::MAX)
            .await?;
        let conversations = self
            .list::<String>("board_removed_conversation", "", u32::MAX)
            .await?;
        remove_from_snapshot(
            snapshot,
            &messages.into_iter().map(|(id, _)| id).collect(),
            &conversations.into_iter().map(|(id, _)| id).collect(),
        );
        Ok(())
    }

    pub async fn filter_removed_message_status(
        &self,
        snapshot: &mut TeamMessageStatusSnapshot,
    ) -> Result<()> {
        let messages = self
            .list::<String>("board_removed_message", "", u32::MAX)
            .await?;
        clear_status_details(snapshot, &messages.into_iter().map(|(id, _)| id).collect());
        Ok(())
    }
}

fn clear_status_details(snapshot: &mut TeamMessageStatusSnapshot, messages: &BTreeSet<String>) {
    for status in &mut snapshot.data {
        if messages.contains(status.message_id.as_str()) {
            status.detail = Some("Permanently removed from Board.".into());
            for recipient in &mut status.recipients {
                if let Some(receipt) = &mut recipient.receipt {
                    receipt.detail = None;
                }
            }
        }
    }
}

fn remove_from_snapshot(
    snapshot: &mut TeamBoardSnapshot,
    messages: &BTreeSet<String>,
    conversations: &BTreeSet<String>,
) {
    snapshot.entries.retain(|entry| {
        !messages.contains(entry.message.id.as_str())
            && !conversations.contains(entry.conversation_id.as_str())
    });
    snapshot
        .followers
        .retain(|follower| !conversations.contains(follower.conversation_id.as_str()));
    snapshot
        .reads
        .retain(|read| !conversations.contains(read.conversation_id.as_str()));
}
