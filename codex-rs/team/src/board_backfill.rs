//! Project historical board messages without changing their envelopes or receipts.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::*;

use crate::board::borrowed_writes;
use crate::board::subscription_key;
use crate::coordinator::Coordinator;
use crate::model::MessageRecord;
use crate::routing::Subscription;

impl Coordinator {
    pub(crate) async fn backfill_board(&self) -> Result<()> {
        let records = self
            .store
            .list::<MessageRecord>("message", "", u32::MAX)
            .await?;
        let board = records
            .into_iter()
            .filter(|(_, record)| record.message.kind == TeamMessageKind::Board)
            .map(|(id, record)| (id, record.message))
            .collect::<HashMap<_, _>>();
        let ids = legacy_order(&board);
        let mut clock = self.store.board_clock().await?;
        let mut writes = Vec::new();
        for id in ids {
            if self.store.board_message_removed(&id).await? {
                continue;
            }
            if self
                .store
                .get::<TeamBoardEntry>("board_entry", &id)
                .await?
                .is_some()
            {
                continue;
            }
            let message = board.get(&id).context("backfill message missing")?;
            let conversation_id = legacy_root(&id, &board);
            let identity = if message.sender.machine_id == "operator" {
                TeamBoardIdentity::User
            } else {
                TeamBoardIdentity::Agent {
                    address: message.sender.clone(),
                }
            };
            clock.append_seq = clock
                .append_seq
                .checked_add(1)
                .context("board append sequence exhausted")?;
            writes.push((
                "board_entry",
                id,
                serde_json::to_value(TeamBoardEntry {
                    message: message.clone(),
                    conversation_id,
                    append_seq: clock.append_seq,
                    author: self.board_author(&identity, true).await?,
                    mentions: Vec::new(),
                    notifications: Vec::new(),
                })?,
            ));
        }
        // Legacy topic subscriptions retain their exact enabled state. A marker
        // makes later new-style changes authoritative across coordinator restart.
        if self
            .store
            .get::<bool>("board_migration", "topics")
            .await?
            .is_none()
        {
            for (_, subscription) in self
                .store
                .list::<Subscription>("subscription", "", u32::MAX)
                .await?
            {
                let identity = TeamBoardIdentity::Agent {
                    address: subscription.sender,
                };
                let key = subscription_key(&subscription.topic, &identity)?;
                let value = subscription.enabled.then_some(TeamBoardTopicSubscription {
                    mode: TeamBoardSubscriptionMode::Wake,
                    topic: subscription.topic,
                    identity,
                });
                writes.push(("board_topic", key, serde_json::to_value(value)?));
            }
            writes.push((
                "board_migration",
                "topics".into(),
                serde_json::to_value(true)?,
            ));
        }
        if !writes.is_empty() {
            clock.revision = clock
                .revision
                .checked_add(1)
                .context("board revision exhausted")?;
            writes.push((
                "board_clock",
                "current".into(),
                serde_json::to_value(clock)?,
            ));
            self.store.put_records(&borrowed_writes(&writes)).await?;
        }
        Ok(())
    }
}

fn legacy_root(id: &str, messages: &HashMap<String, TeamMessage>) -> String {
    let mut visited = HashSet::new();
    let Some(message) = messages.get(id) else {
        return id.into();
    };
    let topic = &message.topic;
    let mut current = message;
    loop {
        if !visited.insert(&current.id) || &current.topic != topic {
            return id.into();
        }
        let Some(parent) = &current.reply_to else {
            return current.id.clone();
        };
        let Some(parent) = messages.get(parent) else {
            return id.into();
        };
        current = parent;
    }
}

// UUID versions and host clocks cannot prove historical coordinator order.
// Preserve known reply dependencies, then use timestamp/ID only to order ready
// historical entries deterministically. Invalid chains remain independent.
fn legacy_order(messages: &HashMap<String, TeamMessage>) -> Vec<String> {
    let mut children = HashMap::<String, Vec<String>>::new();
    let mut ready = BTreeSet::new();
    for (id, message) in messages {
        if legacy_root(id, messages) != *id {
            if let Some(parent) = &message.reply_to {
                children.entry(parent.clone()).or_default().push(id.clone());
            }
        } else {
            ready.insert((message.created_at, id.clone()));
        }
    }
    let mut ordered = Vec::with_capacity(messages.len());
    while let Some((_, id)) = ready.pop_first() {
        if let Some(children) = children.remove(&id) {
            for child in children {
                if let Some(message) = messages.get(&child) {
                    ready.insert((message.created_at, child));
                }
            }
        }
        ordered.push(id);
    }
    ordered
}
