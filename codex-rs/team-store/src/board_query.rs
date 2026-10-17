//! Shared read projection for coordinator queries and committed worker caches.

use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_app_server_protocol::*;

pub fn query_board(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
    request: &TeamBoardParams,
) -> Result<TeamBoardResponse> {
    let revision = snapshot.revision;
    match request {
        TeamBoardParams::List {
            filter,
            query,
            topic,
            cursor,
            limit,
            identity,
        } => {
            let viewer = identity.as_ref().unwrap_or(viewer);
            let query = query.as_ref().map(|query| query.to_lowercase());
            let mut data = conversations(snapshot, viewer)
                .into_iter()
                .filter(|conversation| {
                    let matches_filter = match filter {
                        TeamBoardFilter::All => true,
                        TeamBoardFilter::Following => conversation.following,
                        TeamBoardFilter::Mentions => conversation.mentioned,
                    };
                    matches_filter
                        && topic.as_ref().is_none_or(|topic| {
                            conversation.root.message.topic.as_ref() == Some(topic)
                        })
                        && query.as_ref().is_none_or(|query| {
                            snapshot.entries.iter().any(|entry| {
                                entry.conversation_id == conversation.id
                                    && (entry.message.body.to_lowercase().contains(query)
                                        || entry.author.name.to_lowercase().contains(query)
                                        || entry.message.topic.as_ref().is_some_and(|topic| {
                                            topic.to_lowercase().contains(query)
                                        }))
                            })
                        })
                })
                .collect::<Vec<_>>();
            data.sort_by(|a, b| (b.latest.append_seq, &b.id).cmp(&(a.latest.append_seq, &a.id)));
            if let Some(cursor) = cursor {
                let (expected_revision, sequence, id): (i64, i64, String) =
                    serde_json::from_str(cursor)?;
                ensure!(
                    expected_revision == revision,
                    "board changed; restart the list from its first page"
                );
                data.retain(|entry| {
                    (entry.latest.append_seq, entry.id.as_str()) < (sequence, id.as_str())
                });
            }
            let size = page_size(*limit)?;
            let more = data.len() > size;
            data.truncate(size);
            let next_cursor = if more {
                data.last()
                    .map(|entry| {
                        serde_json::to_string(&(revision, entry.latest.append_seq, &entry.id))
                    })
                    .transpose()?
            } else {
                None
            };
            Ok(TeamBoardResponse::List {
                revision,
                data,
                next_cursor,
            })
        }
        TeamBoardParams::Read {
            conversation_id,
            cursor,
            limit,
            identity,
        } => {
            let conversation = conversations(snapshot, identity.as_ref().unwrap_or(viewer))
                .into_iter()
                .find(|conversation| &conversation.id == conversation_id)
                .context("conversation not found")?;
            let after = cursor
                .as_ref()
                .map(|cursor| cursor.parse::<i64>())
                .transpose()?
                .unwrap_or_default();
            ensure!(after >= 0, "entry cursor must not be negative");
            let mut data = snapshot
                .entries
                .iter()
                .filter(|entry| {
                    &entry.conversation_id == conversation_id && entry.append_seq > after
                })
                .cloned()
                .collect::<Vec<_>>();
            data.sort_by_key(|entry| entry.append_seq);
            let size = page_size(*limit)?;
            let more = data.len() > size;
            data.truncate(size);
            let next_cursor = more
                .then(|| data.last().map(|entry| entry.append_seq.to_string()))
                .flatten();
            Ok(TeamBoardResponse::Read {
                revision,
                conversation: Box::new(conversation),
                data,
                next_cursor,
            })
        }
        TeamBoardParams::Followers {
            conversation_id,
            cursor,
            limit,
        } => {
            ensure!(
                snapshot
                    .entries
                    .iter()
                    .any(|entry| &entry.conversation_id == conversation_id),
                "conversation not found"
            );
            let mut data = snapshot
                .followers
                .iter()
                .filter(|follower| &follower.conversation_id == conversation_id)
                .map(|follower| Ok((serde_json::to_string(&follower.identity)?, follower.clone())))
                .collect::<Result<Vec<_>>>()?;
            data.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(cursor) = cursor {
                data.retain(|(key, _)| key > cursor);
            }
            let size = page_size(*limit)?;
            let more = data.len() > size;
            data.truncate(size);
            let next_cursor = more
                .then(|| data.last().map(|(key, _)| key.clone()))
                .flatten();
            Ok(TeamBoardResponse::Followers {
                revision,
                data: data.into_iter().map(|(_, follower)| follower).collect(),
                next_cursor,
            })
        }
        TeamBoardParams::Topics { cursor, limit } => {
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
                .filter(|topic| cursor.as_ref().is_none_or(|cursor| topic > cursor))
                .collect::<Vec<_>>();
            let size = page_size(*limit)?;
            let more = data.len() > size;
            data.truncate(size);
            let next_cursor = more.then(|| data.last().cloned()).flatten();
            Ok(TeamBoardResponse::Topics {
                revision,
                data,
                next_cursor,
            })
        }
        TeamBoardParams::Post { .. }
        | TeamBoardParams::Reply { .. }
        | TeamBoardParams::Follow { .. }
        | TeamBoardParams::MarkRead { .. }
        | TeamBoardParams::TopicFollow { .. } => {
            bail!("board mutation requires coordinator acceptance")
        }
    }
}

fn page_size(limit: Option<u32>) -> Result<usize> {
    let limit = limit.unwrap_or(50);
    ensure!(limit > 0, "page limit must be positive");
    Ok(limit as usize)
}

fn conversations(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
) -> Vec<TeamBoardConversation> {
    snapshot
        .entries
        .iter()
        .filter(|entry| entry.message.id == entry.conversation_id)
        .map(|root| {
            let entries = snapshot
                .entries
                .iter()
                .filter(|entry| entry.conversation_id == root.conversation_id)
                .collect::<Vec<_>>();
            let latest = entries
                .iter()
                .max_by_key(|entry| entry.append_seq)
                .copied()
                .unwrap_or(root);
            let followers = snapshot
                .followers
                .iter()
                .filter(|follower| follower.conversation_id == root.conversation_id)
                .collect::<Vec<_>>();
            let read_seq = snapshot
                .reads
                .iter()
                .find(|read| {
                    read.conversation_id == root.conversation_id && &read.identity == viewer
                })
                .map_or(0, |read| read.append_seq);
            TeamBoardConversation {
                id: root.conversation_id.clone(),
                root: root.clone(),
                latest: latest.clone(),
                reply_count: entries.len().saturating_sub(1) as u32,
                follower_count: followers.len() as u32,
                following: followers
                    .iter()
                    .any(|follower| &follower.identity == viewer),
                unread: entries
                    .iter()
                    .any(|entry| entry.append_seq > read_seq && &entry.author.identity != viewer),
                mentioned: entries.iter().any(|entry| {
                    entry
                        .mentions
                        .iter()
                        .any(|mention| &mention.identity == viewer)
                }),
                read_seq,
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "board_query_tests.rs"]
mod tests;
