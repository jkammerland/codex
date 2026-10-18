//! Topic previews derive exclusively from committed Board entries and subscriptions.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

/// Return topics in recent coordinator activity order, with exact topic identity.
/// Viewing this projection never advances a conversation's read marker.
pub fn topic_overviews(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
) -> Vec<TeamBoardTopicOverview> {
    let conversations = super::conversations(snapshot, viewer);
    let mut conversations_by_topic = BTreeMap::<&str, Vec<&TeamBoardConversation>>::new();
    for conversation in &conversations {
        if let Some(topic) = conversation.root.message.topic.as_deref() {
            conversations_by_topic
                .entry(topic)
                .or_default()
                .push(conversation);
        }
    }
    let mut entries_by_topic = BTreeMap::<&str, Vec<&TeamBoardEntry>>::new();
    for entry in &snapshot.entries {
        if let Some(topic) = entry.message.topic.as_deref() {
            entries_by_topic.entry(topic).or_default().push(entry);
        }
    }
    let topics = snapshot
        .entries
        .iter()
        .filter_map(|entry| entry.message.topic.clone())
        .chain(
            snapshot
                .topic_subscriptions
                .iter()
                .map(|subscription| subscription.topic.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut data = topics
        .into_iter()
        .map(|topic| {
            let mut recent_conversations = conversations_by_topic
                .remove(topic.as_str())
                .unwrap_or_default();
            recent_conversations
                .sort_by(|a, b| (b.latest.append_seq, &b.id).cmp(&(a.latest.append_seq, &a.id)));
            let conversation_count = recent_conversations.len() as u32;
            let unread_count = recent_conversations
                .iter()
                .filter(|conversation| conversation.unread)
                .count() as u32;
            let mut entries = entries_by_topic.remove(topic.as_str()).unwrap_or_default();
            entries.sort_by_key(|entry| entry.append_seq);
            // Preserve the latest recorded name of each actual author. Subscribers
            // who have never posted are intentionally absent from participants.
            let mut participants = Vec::<TeamBoardAuthor>::new();
            for entry in &entries {
                if let Some(author) = participants
                    .iter_mut()
                    .find(|author| author.identity == entry.author.identity)
                {
                    *author = entry.author.clone();
                } else {
                    participants.push(entry.author.clone());
                }
            }
            let subscription_mode = snapshot
                .topic_subscriptions
                .iter()
                .find(|subscription| {
                    subscription.topic == topic && &subscription.identity == viewer
                })
                .map_or(TeamBoardSubscriptionMode::Off, |subscription| {
                    subscription.mode
                });
            recent_conversations.truncate(3);
            TeamBoardTopicOverview {
                topic,
                recent_conversations: recent_conversations
                    .into_iter()
                    .map(|conversation| TeamBoardConversationPreview {
                        id: conversation.id.clone(),
                        root: entry_preview(&conversation.root),
                        latest: entry_preview(&conversation.latest),
                        reply_count: conversation.reply_count,
                        unread: conversation.unread,
                    })
                    .collect(),
                conversation_count,
                message_count: entries.len() as u32,
                participants,
                latest: entries.last().map(|entry| entry_preview(entry)),
                unread_count,
                subscription_mode,
            }
        })
        .collect::<Vec<_>>();
    data.sort_by(|a, b| {
        (
            b.latest.as_ref().map_or(0, |entry| entry.append_seq),
            &b.topic,
        )
            .cmp(&(
                a.latest.as_ref().map_or(0, |entry| entry.append_seq),
                &a.topic,
            ))
    });
    data
}

pub(super) fn query_topic_overviews(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
    cursor: Option<&str>,
    limit: Option<u32>,
) -> Result<TeamBoardResponse> {
    let mut data = topic_overviews(snapshot, viewer);
    if let Some(cursor) = cursor {
        let (revision, sequence, topic): (i64, i64, String) = serde_json::from_str(cursor)?;
        ensure!(
            revision == snapshot.revision,
            "board changed; restart topics from the first page"
        );
        data.retain(|entry| {
            (
                entry.latest.as_ref().map_or(0, |latest| latest.append_seq),
                entry.topic.as_str(),
            ) < (sequence, topic.as_str())
        });
    }
    let size = super::page_size(limit)?;
    let more = data.len() > size;
    data.truncate(size);
    let next_cursor = if more {
        data.last()
            .map(|entry| {
                serde_json::to_string(&(
                    snapshot.revision,
                    entry.latest.as_ref().map_or(0, |latest| latest.append_seq),
                    &entry.topic,
                ))
            })
            .transpose()?
    } else {
        None
    };
    Ok(TeamBoardResponse::TopicOverview {
        revision: snapshot.revision,
        data,
        next_cursor,
    })
}

fn entry_preview(entry: &TeamBoardEntry) -> TeamBoardEntryPreview {
    let line = entry
        .message
        .body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut characters = line.chars();
    let mut excerpt = characters.by_ref().take(240).collect::<String>();
    if characters.next().is_some() {
        excerpt.pop();
        excerpt.push('…');
    }
    TeamBoardEntryPreview {
        message_id: entry.message.id.clone(),
        conversation_id: entry.conversation_id.clone(),
        append_seq: entry.append_seq,
        author: entry.author.clone(),
        created_at: entry.message.created_at,
        excerpt,
    }
}
