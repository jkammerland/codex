//! Shared read projection for coordinator queries and committed worker caches.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_app_server_protocol::*;

#[path = "board_navigation.rs"]
mod navigation;
#[path = "board_paging.rs"]
mod paging;
#[path = "board_topics.rs"]
mod topics;
pub use navigation::query_agent_board;
pub use navigation::validate_board_acknowledgement;
pub use topics::topic_overviews;

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
            let scope = serde_json::to_string(&("list", filter, query, topic))?;
            let membership = if *filter == TeamBoardFilter::Following {
                paging::Membership::Following
            } else {
                paging::Membership::None
            };
            let (position, prefix) =
                paging::PageCursor::open(snapshot, viewer, scope, cursor.as_deref(), membership)?;
            let mut data = conversations(&prefix, viewer)
                .into_iter()
                .filter(|conversation| {
                    let matches_filter = match filter {
                        TeamBoardFilter::All => true,
                        TeamBoardFilter::Following => {
                            conversation.following
                                || conversation.topic_subscription_mode
                                    != TeamBoardSubscriptionMode::Off
                        }
                        TeamBoardFilter::Mentions => conversation.mentioned,
                        TeamBoardFilter::Unread => conversation.unread,
                    };
                    let entries = prefix
                        .entries
                        .iter()
                        .filter(|entry| entry.conversation_id == conversation.id)
                        .collect::<Vec<_>>();
                    matches_filter
                        && topic.as_ref().is_none_or(|topic| {
                            conversation.root.message.topic.as_ref() == Some(topic)
                        })
                        && query.as_ref().is_none_or(|query| {
                            team_board_matches_query(
                                conversation.root.message.topic.as_deref(),
                                &entries,
                                query,
                            )
                        })
                })
                .collect::<Vec<_>>();
            data.sort_by(|a, b| (b.latest.append_seq, &b.id).cmp(&(a.latest.append_seq, &a.id)));
            if cursor.is_some() {
                data.retain(|entry| {
                    (entry.latest.append_seq, entry.id.as_str())
                        < (position.sequence, position.key.as_str())
                });
            }
            let size = page_size(*limit)?;
            let more = data.len() > size;
            data.truncate(size);
            let next_cursor = if more {
                data.last()
                    .map(|entry| position.after(entry.latest.append_seq, &entry.id))
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
        TeamBoardParams::ReadPage { .. } => navigation::read_page(snapshot, viewer, request),
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
        TeamBoardParams::TopicOverview {
            cursor,
            limit,
            identity,
        } => topics::query_topic_overviews(
            snapshot,
            identity.as_ref().unwrap_or(viewer),
            cursor.as_deref(),
            *limit,
        ),
        TeamBoardParams::Post { .. }
        | TeamBoardParams::Reply { .. }
        | TeamBoardParams::Follow { .. }
        | TeamBoardParams::MarkRead { .. }
        | TeamBoardParams::Acknowledge { .. }
        | TeamBoardParams::TopicFollow { .. }
        | TeamBoardParams::SubscriptionSet { .. } => {
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
    let mut grouped = BTreeMap::<&str, Vec<&TeamBoardEntry>>::new();
    for entry in &snapshot.entries {
        grouped
            .entry(&entry.conversation_id)
            .or_default()
            .push(entry);
    }
    let mut followers_by_conversation = BTreeMap::<&str, Vec<&TeamBoardFollower>>::new();
    for follower in &snapshot.followers {
        followers_by_conversation
            .entry(&follower.conversation_id)
            .or_default()
            .push(follower);
    }
    let read_markers = snapshot
        .reads
        .iter()
        .filter(|read| &read.identity == viewer)
        .map(|read| (read.conversation_id.as_str(), read.append_seq))
        .collect::<BTreeMap<_, _>>();
    let topic_modes = snapshot
        .topic_subscriptions
        .iter()
        .filter(|subscription| &subscription.identity == viewer)
        .map(|subscription| (subscription.topic.as_str(), subscription.mode))
        .collect::<BTreeMap<_, _>>();
    snapshot
        .entries
        .iter()
        .filter(|entry| entry.message.id == entry.conversation_id)
        .map(|root| {
            let entries = &grouped[root.conversation_id.as_str()];
            let latest = entries
                .iter()
                .max_by_key(|entry| entry.append_seq)
                .copied()
                .unwrap_or(root);
            let followers = followers_by_conversation
                .get(root.conversation_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let read_seq = read_markers
                .get(root.conversation_id.as_str())
                .copied()
                .unwrap_or_default();
            let subscription_mode = followers
                .iter()
                .find(|follower| &follower.identity == viewer)
                .map_or(TeamBoardSubscriptionMode::Off, |follower| follower.mode);
            let topic_subscription_mode = root
                .message
                .topic
                .as_deref()
                .and_then(|topic| topic_modes.get(topic))
                .copied()
                .unwrap_or_default();
            TeamBoardConversation {
                id: root.conversation_id.clone(),
                root: root.clone(),
                latest: latest.clone(),
                reply_count: entries.len().saturating_sub(1) as u32,
                follower_count: followers.len() as u32,
                following: subscription_mode != TeamBoardSubscriptionMode::Off,
                subscription_mode,
                topic_subscription_mode,
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
