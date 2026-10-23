//! Agent pages expose contiguous read coverage without acknowledging on fetch.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde::Deserialize;
use serde::Serialize;

use super::paging::Membership;
use super::paging::PageCursor;
use super::topics::entry_preview;

#[derive(Deserialize, Serialize)]
struct Acknowledgement {
    version: u8,
    reader: TeamBoardIdentity,
    conversation: String,
    cutoff: i64,
    first: String,
    last: String,
    predecessor: i64,
    through: i64,
}

/// Compact projection used only by the agent interface; operator reads retain
/// their existing complete conversation representation.
pub fn query_agent_board(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
    request: &TeamBoardParams,
) -> Result<TeamBoardResponse> {
    let mut request = request.clone();
    match &mut request {
        TeamBoardParams::List { limit, .. }
        | TeamBoardParams::ReadPage { limit, .. }
        | TeamBoardParams::TopicOverview { limit, .. }
        | TeamBoardParams::Followers { limit, .. }
        | TeamBoardParams::Topics { limit, .. } => {
            limit.get_or_insert(20);
        }
        _ => {}
    }
    let response = super::query_board(snapshot, viewer, &request)?;
    let TeamBoardResponse::List {
        revision,
        data,
        next_cursor,
    } = response
    else {
        return Ok(response);
    };
    let TeamBoardParams::List {
        query,
        cursor,
        identity,
        ..
    } = request
    else {
        unreachable!("only a list request produces a list response");
    };
    let reader = identity.as_ref().unwrap_or(viewer);
    // Match excerpts and counts must use the same content cutoff as discovery.
    let cutoff = cursor
        .as_deref()
        .map(serde_json::from_str::<PageCursor>)
        .transpose()?
        .map_or(i64::MAX, |cursor| cursor.cutoff);
    let data = data
        .into_iter()
        .map(|conversation| {
            let mut entries = snapshot
                .entries
                .iter()
                .filter(|entry| {
                    entry.conversation_id == conversation.id && entry.append_seq <= cutoff
                })
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.append_seq);
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
            let words = query
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .filter(|word| !word.starts_with('#'))
                .map(str::to_lowercase)
                .collect::<Vec<_>>();
            // One hit per term makes every search term navigable without returning
            // the complete contents of every matching entry in a long thread.
            let mut matches = Vec::<TeamBoardEntryPreview>::new();
            for word in words {
                if let Some(entry) = entries.iter().find(|entry| {
                    entry.message.body.to_lowercase().contains(&word)
                        || entry.author.name.to_lowercase().contains(&word)
                }) && !matches.iter().any(|hit| hit.message_id == entry.message.id)
                {
                    let mut hit = entry_preview(entry);
                    if let Some(position) = entry.message.body.to_lowercase().find(&word) {
                        // Case folding can expand a Unicode character. Map the
                        // lower-case byte offset back before selecting context.
                        let mut folded_bytes = 0;
                        let mut character = 0_usize;
                        for value in entry.message.body.chars() {
                            let width = value.to_lowercase().map(char::len_utf8).sum::<usize>();
                            if folded_bytes + width > position {
                                break;
                            }
                            folded_bytes += width;
                            character += 1;
                        }
                        let start = character.saturating_sub(60);
                        let mut characters = entry.message.body.chars().skip(start);
                        let excerpt = characters.by_ref().take(240).collect::<String>();
                        hit.excerpt = format!(
                            "{}{}{}",
                            if start > 0 { "…" } else { "" },
                            excerpt,
                            if characters.next().is_some() {
                                "…"
                            } else {
                                ""
                            }
                        );
                    }
                    matches.push(hit);
                }
            }
            TeamBoardThreadSummary {
                id: conversation.id,
                topic: conversation.root.message.topic.clone(),
                title: entry_preview(&conversation.root).excerpt,
                latest: entry_preview(&conversation.latest),
                participants,
                reply_count: conversation.reply_count,
                unread_count: entries
                    .iter()
                    .filter(|entry| {
                        entry.append_seq > conversation.read_seq && &entry.author.identity != reader
                    })
                    .count() as u32,
                read_seq: conversation.read_seq,
                subscription_mode: conversation.subscription_mode,
                topic_subscription_mode: conversation.topic_subscription_mode,
                matches,
            }
        })
        .collect();
    Ok(TeamBoardResponse::Threads {
        revision,
        data,
        next_cursor,
    })
}

pub(super) fn read_page(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
    request: &TeamBoardParams,
) -> Result<TeamBoardResponse> {
    let TeamBoardParams::ReadPage {
        conversation_id,
        mode,
        anchor_message_id,
        cursor,
        limit,
        identity,
    } = request
    else {
        unreachable!("read_page only handles ReadPage");
    };
    let viewer = identity.as_ref().unwrap_or(viewer);
    // Continuations carry their first-page mode and anchor. Callers need only
    // retain the opaque cursor and conversation, not reconstruct its options.
    let (mode, anchor_message_id) = if let Some(cursor) = cursor {
        let state: PageCursor = serde_json::from_str(cursor)?;
        let (operation, conversation, mode, anchor): (
            String,
            String,
            TeamBoardReadMode,
            Option<String>,
        ) = serde_json::from_str(&state.scope)?;
        ensure!(
            operation == "readPage" && conversation == *conversation_id,
            "cursor belongs to another conversation"
        );
        (mode, anchor)
    } else {
        (*mode, anchor_message_id.clone())
    };
    ensure!(
        (mode == TeamBoardReadMode::Around) == anchor_message_id.is_some(),
        "around reads require anchorMessageId; other modes do not accept it"
    );
    let scope = serde_json::to_string(&("readPage", conversation_id, mode, &anchor_message_id))?;
    let (position, prefix) =
        PageCursor::open(snapshot, viewer, scope, cursor.as_deref(), Membership::None)?;
    let conversation = super::conversations(&prefix, viewer)
        .into_iter()
        .find(|conversation| &conversation.id == conversation_id)
        .context("conversation not found")?;
    let mut entries = prefix
        .entries
        .iter()
        .filter(|entry| &entry.conversation_id == conversation_id)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.append_seq);
    let size = super::page_size(Some(limit.unwrap_or(20)))?;
    let start = if cursor.is_some() {
        entries.partition_point(|entry| entry.append_seq <= position.sequence)
    } else {
        match mode {
            TeamBoardReadMode::Unread => {
                entries.partition_point(|entry| entry.append_seq <= conversation.read_seq)
            }
            TeamBoardReadMode::History => 0,
            TeamBoardReadMode::Around => entries
                .iter()
                .position(|entry| Some(&entry.message.id) == anchor_message_id.as_ref())
                .context("anchor message not found in conversation")?
                .saturating_sub(size / 2),
        }
    };
    let end = start.saturating_add(size).min(entries.len());
    let page = &entries[start..end];
    let next_cursor = if end < entries.len() {
        page.last()
            .map(|entry| position.after(entry.append_seq, &entry.message.id))
            .transpose()?
    } else {
        None
    };
    let acknowledgement_token = if let (Some(first), Some(last)) = (page.first(), page.last()) {
        Some(serde_json::to_string(&Acknowledgement {
            version: 1,
            reader: viewer.clone(),
            conversation: conversation_id.clone(),
            cutoff: position.cutoff,
            first: first.message.id.clone(),
            last: last.message.id.clone(),
            predecessor: start
                .checked_sub(1)
                .map_or(0, |index| entries[index].append_seq),
            through: last.append_seq,
        })?)
    } else {
        None
    };
    let root = (cursor.is_none()
        && !page
            .iter()
            .any(|entry| entry.message.id == *conversation_id))
    .then(|| entry_preview(&conversation.root));
    Ok(TeamBoardResponse::ReadPage {
        page: Box::new(TeamBoardReadPage {
            revision: snapshot.revision,
            snapshot_through: position.cutoff,
            conversation_id: conversation_id.clone(),
            topic: conversation.root.message.topic,
            root,
            read_seq: conversation.read_seq,
            unread_count: entries
                .iter()
                .filter(|entry| {
                    entry.append_seq > conversation.read_seq && &entry.author.identity != viewer
                })
                .count() as u32,
            subscription_mode: conversation.subscription_mode,
            topic_subscription_mode: conversation.topic_subscription_mode,
            data: page
                .iter()
                .map(|entry| TeamBoardReadEntry {
                    id: entry.message.id.clone(),
                    append_seq: entry.append_seq,
                    author: entry.author.clone(),
                    reply_to: entry.message.reply_to.clone(),
                    created_at: entry.message.created_at,
                    body: entry.message.body.clone(),
                })
                .collect(),
            next_cursor,
            acknowledgement_token,
        }),
    })
}

/// Validate coverage against the committed conversation and the authenticated
/// reader. Global append gaps are valid; a missing conversation predecessor is not.
pub fn validate_board_acknowledgement(
    snapshot: &TeamBoardSnapshot,
    viewer: &TeamBoardIdentity,
    token: &str,
) -> Result<TeamBoardReadState> {
    let token: Acknowledgement =
        serde_json::from_str(token).context("invalid read acknowledgement token")?;
    ensure!(
        token.version == 1 && token.reader == *viewer,
        "acknowledgement belongs to another reader"
    );
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|entry| {
            entry.conversation_id == token.conversation && entry.append_seq <= token.cutoff
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.append_seq);
    ensure!(
        entries
            .first()
            .is_some_and(|entry| entry.message.id == token.conversation),
        "conversation not found or removed"
    );
    let first = entries
        .iter()
        .position(|entry| entry.message.id == token.first)
        .context("acknowledgement entry not found")?;
    let last = entries
        .iter()
        .position(|entry| entry.message.id == token.last)
        .context("acknowledgement entry not found")?;
    ensure!(
        first <= last
            && entries[last].append_seq == token.through
            && first
                .checked_sub(1)
                .map_or(0, |index| entries[index].append_seq)
                == token.predecessor,
        "acknowledgement does not describe a contiguous page"
    );
    let previous = snapshot
        .reads
        .iter()
        .find(|read| read.conversation_id == token.conversation && &read.identity == viewer)
        .map_or(0, |read| read.append_seq);
    ensure!(
        previous >= token.predecessor,
        "unread gap before this page; acknowledge earlier pages first"
    );
    Ok(TeamBoardReadState {
        conversation_id: token.conversation,
        identity: viewer.clone(),
        append_seq: previous.max(token.through),
    })
}

#[cfg(test)]
#[path = "board_navigation_tests.rs"]
mod tests;
