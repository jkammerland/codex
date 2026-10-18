//! Board metadata stays in the same SQLite transaction as delivery effects.

use anyhow::Result;
use codex_app_server_protocol::*;
use serde::Deserialize;
use serde::Serialize;

use crate::TeamStore;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardClock {
    pub revision: i64,
    pub append_seq: i64,
}

impl TeamStore {
    pub async fn board_clock(&self) -> Result<BoardClock> {
        Ok(self
            .get("board_clock", "current")
            .await?
            .unwrap_or_default())
    }

    /// Read committed projection rows in coordinator append order. The record
    /// indexes also serve per-conversation cursors without machine-clock sorting.
    pub async fn board_entries(
        &self,
        conversation: Option<&str>,
        after: i64,
    ) -> Result<Vec<TeamBoardEntry>> {
        let rows: Vec<String> = match conversation {
            Some(conversation) => sqlx::query_scalar("SELECT payload FROM record WHERE namespace = 'board_entry' AND json_extract(payload, '$.conversationId') = ? AND json_extract(payload, '$.appendSeq') > ? ORDER BY json_extract(payload, '$.appendSeq')")
                .bind(conversation).bind(after).fetch_all(&self.pool).await?,
            None => sqlx::query_scalar("SELECT payload FROM record WHERE namespace = 'board_entry' AND json_extract(payload, '$.appendSeq') > ? ORDER BY json_extract(payload, '$.appendSeq')")
                .bind(after).fetch_all(&self.pool).await?,
        };
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(Into::into))
            .collect()
    }

    /// Callers that need a consistent snapshot serialize this with mutations.
    pub async fn board_snapshot(&self) -> Result<TeamBoardSnapshot> {
        Ok(TeamBoardSnapshot {
            revision: self.board_clock().await?.revision,
            entries: self.board_entries(None, 0).await?,
            followers: self
                .list::<Option<TeamBoardFollower>>("board_follower", "", u32::MAX)
                .await?
                .into_iter()
                .filter_map(|(_, follower)| follower)
                .collect(),
            reads: self
                .list("board_read", "", u32::MAX)
                .await?
                .into_iter()
                .map(|(_, read)| read)
                .collect(),
            topic_subscriptions: self
                .list::<Option<TeamBoardTopicSubscription>>("board_topic", "", u32::MAX)
                .await?
                .into_iter()
                .filter_map(|(_, subscription)| subscription)
                .collect(),
            members: Vec::new(),
        })
    }
}

#[cfg(test)]
#[path = "board_migration_tests.rs"]
mod migration_tests;
