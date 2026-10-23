//! Compact agent navigation preserves explicit, contiguous read acknowledgement.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

use super::TeamBoardAuthor;
use super::TeamBoardEntry;
use super::TeamBoardEntryPreview;
use super::TeamBoardSubscriptionMode;

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum TeamBoardReadMode {
    #[default]
    Unread,
    History,
    Around,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardThreadSummary {
    pub id: String,
    pub topic: Option<String>,
    pub title: String,
    pub latest: TeamBoardEntryPreview,
    pub participants: Vec<TeamBoardAuthor>,
    pub reply_count: u32,
    pub unread_count: u32,
    #[ts(type = "number")]
    pub read_seq: i64,
    pub subscription_mode: TeamBoardSubscriptionMode,
    pub topic_subscription_mode: TeamBoardSubscriptionMode,
    pub matches: Vec<TeamBoardEntryPreview>,
}

/// Full entry bodies occur only here, never repeated in the thread header.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardReadEntry {
    pub id: String,
    #[ts(type = "number")]
    pub append_seq: i64,
    pub author: TeamBoardAuthor,
    pub reply_to: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    pub body: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamBoardReadPage {
    #[ts(type = "number")]
    pub revision: i64,
    #[ts(type = "number")]
    pub snapshot_through: i64,
    pub conversation_id: String,
    pub topic: Option<String>,
    pub root: Option<TeamBoardEntryPreview>,
    #[ts(type = "number")]
    pub read_seq: i64,
    pub unread_count: u32,
    pub subscription_mode: TeamBoardSubscriptionMode,
    pub topic_subscription_mode: TeamBoardSubscriptionMode,
    pub data: Vec<TeamBoardReadEntry>,
    pub next_cursor: Option<String>,
    pub acknowledgement_token: Option<String>,
}

/// Search terms are ANDed across a conversation; `#topic` matches its exact topic.
/// Both agent discovery and the terminal Board use this interpretation.
pub fn team_board_matches_query(
    topic: Option<&str>,
    entries: &[&TeamBoardEntry],
    query: &str,
) -> bool {
    query.split_whitespace().all(|word| {
        if let Some(expected) = word.strip_prefix('#') {
            topic.is_some_and(|topic| topic.trim_start_matches('#').eq_ignore_ascii_case(expected))
        } else {
            let word = word.to_lowercase();
            entries.iter().any(|entry| {
                entry.message.body.to_lowercase().contains(&word)
                    || entry.author.name.to_lowercase().contains(&word)
            })
        }
    })
}
