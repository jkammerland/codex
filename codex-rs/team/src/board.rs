//! Conversation actions project onto the existing durable message/outbox ledger.

use std::collections::BTreeMap;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::model::*;

pub(crate) type BoardWrites = Vec<(&'static str, String, Value)>;

pub(crate) fn borrowed_writes(writes: &BoardWrites) -> Vec<(&str, &str, Value)> {
    writes
        .iter()
        .map(|(namespace, key, value)| (*namespace, key.as_str(), value.clone()))
        .collect()
}

pub(crate) fn identity_key(identity: &TeamBoardIdentity) -> Result<String> {
    Ok(serde_json::to_string(identity)?)
}

pub(crate) fn subscription_key(conversation: &str, identity: &TeamBoardIdentity) -> Result<String> {
    Ok(serde_json::to_string(&(conversation, identity))?)
}

pub(crate) fn is_board_query(request: &TeamBoardParams) -> bool {
    matches!(
        request,
        TeamBoardParams::List { .. }
            | TeamBoardParams::Read { .. }
            | TeamBoardParams::Followers { .. }
            | TeamBoardParams::Topics { .. }
            | TeamBoardParams::TopicOverview { .. }
    )
}

impl Coordinator {
    pub(crate) async fn board_author(
        &self,
        identity: &TeamBoardIdentity,
        legacy: bool,
    ) -> Result<TeamBoardAuthor> {
        match identity {
            TeamBoardIdentity::User => Ok(TeamBoardAuthor {
                identity: identity.clone(),
                name: "User".into(),
                machine_name: None,
                legacy,
            }),
            TeamBoardIdentity::Agent { address } => {
                let agent = self
                    .store
                    .get::<Agent>("agent", &address_key(address))
                    .await?;
                ensure!(legacy || agent.is_some(), "board agent is not enrolled");
                let name = if legacy {
                    format!("Legacy agent {}", address.thread_id)
                } else {
                    agent
                        .context("board agent is not enrolled")?
                        .enrollment
                        .alias
                };
                let machine_name = self
                    .manifest
                    .machines
                    .iter()
                    .find(|machine| machine.id == address.machine_id)
                    .map(|machine| machine.label.clone())
                    .unwrap_or_else(|| address.machine_id.clone());
                Ok(TeamBoardAuthor {
                    identity: identity.clone(),
                    name,
                    machine_name: Some(machine_name),
                    legacy,
                })
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "board revision and its projection must come from the same committed mutation"
    )]
    pub(crate) async fn board_snapshot(&self) -> Result<TeamBoardSnapshot> {
        let _mutation = self.mutations.lock().await;
        self.board_snapshot_unlocked().await
    }

    async fn board_snapshot_unlocked(&self) -> Result<TeamBoardSnapshot> {
        let mut snapshot = self.store.board_snapshot().await?;
        snapshot.members.push(TeamBoardMember {
            author: self.board_author(&TeamBoardIdentity::User, false).await?,
            available: true,
        });
        let statuses = self.statuses.read().await.clone();
        for agent in self.agents().await? {
            let address = agent.enrollment.address;
            let available = statuses
                .get(&server_key(&address.machine_id, &address.server_id))
                .is_some_and(|status| status.connected)
                && agent.status != "archived";
            let identity = TeamBoardIdentity::Agent { address };
            snapshot.members.push(TeamBoardMember {
                author: self.board_author(&identity, false).await?,
                available,
            });
        }
        for follower in &mut snapshot.followers {
            if let Some(member) = snapshot
                .members
                .iter()
                .find(|member| member.author.identity == follower.identity)
            {
                follower.author.clone_from(&member.author);
            }
        }
        Ok(snapshot)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "board snapshots and effects share one coordinator mutation order"
    )]
    pub(crate) async fn execute_board(
        &self,
        request: TeamBoardParams,
        operation: Option<(&str, &str)>,
    ) -> Result<TeamResponse> {
        let _mutation = self.mutations.lock().await;
        if is_board_query(&request) {
            return Ok(TeamResponse::Board {
                response: codex_team_store::query_board(
                    &self.board_snapshot_unlocked().await?,
                    &TeamBoardIdentity::User,
                    &request,
                )?,
            });
        }
        if let Some((id, digest)) = operation
            && let Some(response) = self.operation_result(id, digest).await?
        {
            return Ok(response);
        }
        let id = operation.map_or_else(|| Uuid::now_v7().to_string(), |(id, _)| id.to_string());
        let rejection_digest = match operation {
            Some((_, digest)) => digest.to_string(),
            None => serde_json::to_string(&format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&TeamRequest::Board {
                    request: request.clone()
                })?)
            ))?,
        };
        let (response, mut writes) = match self
            .prepare_board_request(request, TeamBoardIdentity::User, &id)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                // A definitive rejection must survive concurrent or delayed
                // retries even if enrollment or other validation inputs change.
                // Failure to commit this terminal outcome remains uncertain.
                let rejected = crate::TeamOperationRejected {
                    operation_id: id.clone(),
                    reason: error.to_string(),
                };
                self.store
                    .put_record_once(
                        "operator_operation",
                        &id,
                        &rejection_digest,
                        &[("operator_rejection", &id, serde_json::to_value(rejected)?)],
                    )
                    .await?;
                return self
                    .operation_result(&id, &rejection_digest)
                    .await?
                    .context("board operation outcome is missing");
            }
        };
        let response = TeamResponse::Board { response };
        if let Some((id, digest)) = operation {
            writes.push((
                "operator_result",
                id.into(),
                serde_json::to_value(&response)?,
            ));
            self.store
                .put_record_once("operator_operation", id, digest, &borrowed_writes(&writes))
                .await?;
        } else {
            self.store.put_records(&borrowed_writes(&writes)).await?;
        }
        self.notify();
        Ok(response)
    }

    pub(crate) async fn prepare_board_request(
        &self,
        request: TeamBoardParams,
        actor: TeamBoardIdentity,
        id: &str,
    ) -> Result<(TeamBoardResponse, BoardWrites)> {
        let mut clock = self.store.board_clock().await?;
        let mut writes = Vec::new();
        let mut conversation_id = None;
        match request {
            TeamBoardParams::Post {
                topic,
                body,
                mentions,
            } => {
                let mut message = self
                    .operator_message(None, body, TeamMessageKind::Board, None, Some(topic))
                    .await?;
                message.id = Uuid::parse_str(id)?.to_string();
                if let TeamBoardIdentity::Agent { address } = &actor {
                    message.sender = address.clone();
                }
                let (record, effects) = self
                    .prepare_message_effects(message, TeamDeliveryMode::NextTurn, mentions)
                    .await?;
                conversation_id = Some(record.message.id);
                writes = effects;
            }
            TeamBoardParams::Reply {
                conversation_id: conversation,
                reply_to,
                body,
                mentions,
            } => {
                let root = self
                    .store
                    .get::<TeamBoardEntry>("board_entry", &conversation)
                    .await?
                    .context("conversation not found")?;
                ensure!(
                    root.conversation_id == conversation,
                    "conversation ID must identify its root post"
                );
                let reply_to = reply_to.unwrap_or_else(|| conversation.clone());
                let parent = self
                    .store
                    .get::<TeamBoardEntry>("board_entry", &reply_to)
                    .await?
                    .context("reply target not found")?;
                ensure!(
                    parent.conversation_id == conversation,
                    "reply target belongs to another conversation"
                );
                let mut message = self
                    .operator_message(
                        None,
                        body,
                        TeamMessageKind::Board,
                        Some(reply_to),
                        root.message.topic,
                    )
                    .await?;
                message.id = Uuid::parse_str(id)?.to_string();
                if let TeamBoardIdentity::Agent { address } = &actor {
                    message.sender = address.clone();
                }
                let (_, effects) = self
                    .prepare_message_effects(message, TeamDeliveryMode::NextTurn, mentions)
                    .await?;
                conversation_id = Some(conversation);
                writes = effects;
            }
            TeamBoardParams::Follow {
                conversation_id: conversation,
                identity,
                enabled,
            } => {
                let identity = requested_identity(&actor, identity)?;
                writes = self
                    .prepare_board_subscription(
                        TeamBoardSubscriptionTarget::Conversation {
                            conversation_id: conversation.clone(),
                        },
                        identity,
                        crate::board_subscriptions::SubscriptionChange::Legacy(enabled),
                    )
                    .await?;
                conversation_id = Some(conversation);
            }
            TeamBoardParams::MarkRead {
                conversation_id: conversation,
                identity,
                append_seq,
            } => {
                let identity = requested_identity(&actor, identity)?;
                self.require_conversation(&conversation).await?;
                ensure!(append_seq >= 0, "read sequence must not be negative");
                let latest = self
                    .store
                    .board_entries(Some(&conversation), 0)
                    .await?
                    .into_iter()
                    .map(|entry| entry.append_seq)
                    .max()
                    .unwrap_or_default();
                ensure!(
                    append_seq <= latest,
                    "read sequence exceeds the conversation history"
                );
                let key = subscription_key(&conversation, &identity)?;
                let previous = self
                    .store
                    .get::<TeamBoardReadState>("board_read", &key)
                    .await?
                    .map_or(0, |read| read.append_seq);
                writes.push((
                    "board_read",
                    key,
                    serde_json::to_value(TeamBoardReadState {
                        conversation_id: conversation.clone(),
                        identity,
                        append_seq: append_seq.max(previous),
                    })?,
                ));
                conversation_id = Some(conversation);
            }
            TeamBoardParams::TopicFollow {
                topic,
                identity,
                enabled,
            } => {
                let identity = requested_identity(&actor, identity)?;
                writes = self
                    .prepare_board_subscription(
                        TeamBoardSubscriptionTarget::Topic { topic },
                        identity,
                        crate::board_subscriptions::SubscriptionChange::Legacy(enabled),
                    )
                    .await?;
            }
            TeamBoardParams::SubscriptionSet {
                target,
                identity,
                mode,
            } => {
                let identity = requested_identity(&actor, identity)?;
                if let TeamBoardSubscriptionTarget::Conversation {
                    conversation_id: conversation,
                } = &target
                {
                    conversation_id = Some(conversation.clone());
                }
                writes = self
                    .prepare_board_subscription(
                        target,
                        identity,
                        crate::board_subscriptions::SubscriptionChange::Explicit(mode),
                    )
                    .await?;
            }
            TeamBoardParams::List { .. }
            | TeamBoardParams::Read { .. }
            | TeamBoardParams::Followers { .. }
            | TeamBoardParams::Topics { .. }
            | TeamBoardParams::TopicOverview { .. } => {
                anyhow::bail!("read queries cannot enter the board outbox")
            }
        }
        // Message preparation has already allocated its append and revision.
        if let Some((_, _, value)) = writes
            .iter()
            .find(|(namespace, _, _)| *namespace == "board_clock")
        {
            clock = serde_json::from_value(value.clone())?;
        } else {
            clock.revision = clock
                .revision
                .checked_add(1)
                .context("board revision exhausted")?;
            writes.push((
                "board_clock",
                "current".into(),
                serde_json::to_value(&clock)?,
            ));
        }
        Ok((
            TeamBoardResponse::Mutation {
                revision: clock.revision,
                state: TeamBoardMutationState::Applied,
                id: id.into(),
                conversation_id,
            },
            writes,
        ))
    }

    pub(crate) async fn require_conversation(&self, conversation: &str) -> Result<()> {
        let root = self
            .store
            .get::<TeamBoardEntry>("board_entry", conversation)
            .await?
            .context("conversation not found")?;
        ensure!(
            root.conversation_id == conversation,
            "conversation ID must identify its root post"
        );
        Ok(())
    }

    /// Prepare immutable attribution and a frozen audience in the same commit as
    /// the original message. Execution controls affect delivery, not visibility.
    pub(crate) async fn prepare_message_effects(
        &self,
        message: TeamMessage,
        delivery: TeamDeliveryMode,
        mentions: Vec<TeamBoardMention>,
    ) -> Result<(MessageRecord, BoardWrites)> {
        let mut record = self.prepare_message(message, delivery).await?;
        let mut writes = Vec::new();
        if record.message.kind == TeamMessageKind::Board {
            if let Some(existing) = self
                .store
                .get::<TeamBoardEntry>("board_entry", &record.message.id)
                .await?
            {
                ensure!(
                    existing.mentions == mentions,
                    "message identity has conflicting mentions"
                );
                return Ok((record, writes));
            }
            validate_mentions(&record.message.body, &mentions)?;
            for mention in &mentions {
                self.board_author(&mention.identity, false).await?;
            }
            let conversation_id = if let Some(parent) = &record.message.reply_to {
                let parent = self
                    .store
                    .get::<TeamBoardEntry>("board_entry", parent)
                    .await?
                    .context("board reply target is missing")?;
                ensure!(
                    parent.message.topic == record.message.topic,
                    "board replies must retain their conversation topic"
                );
                parent.conversation_id
            } else {
                record.message.id.clone()
            };
            let identity = if record.message.sender.machine_id == "operator" {
                TeamBoardIdentity::User
            } else {
                TeamBoardIdentity::Agent {
                    address: record.message.sender.clone(),
                }
            };
            let author = self.board_author(&identity, false).await?;
            let mut clock = self.store.board_clock().await?;
            clock.append_seq = clock
                .append_seq
                .checked_add(1)
                .context("board append sequence exhausted")?;
            clock.revision = clock
                .revision
                .checked_add(1)
                .context("board revision exhausted")?;
            let notifications = self
                .board_notifications(&record.message, &conversation_id, &identity, &mentions)
                .await?;
            record.targets = Some(
                notifications
                    .iter()
                    .filter_map(|notification| match &notification.identity {
                        TeamBoardIdentity::User => None,
                        TeamBoardIdentity::Agent { address } => Some(address.clone()),
                    })
                    .collect(),
            );
            record.published = true;
            let entry = TeamBoardEntry {
                message: record.message.clone(),
                conversation_id: conversation_id.clone(),
                append_seq: clock.append_seq,
                author: author.clone(),
                mentions,
                notifications,
            };
            writes.push((
                "board_entry",
                record.message.id.clone(),
                serde_json::to_value(entry)?,
            ));
            writes.push((
                "board_clock",
                "current".into(),
                serde_json::to_value(clock)?,
            ));
            if identity == TeamBoardIdentity::User {
                writes.push((
                    "board_follower",
                    subscription_key(&conversation_id, &identity)?,
                    serde_json::to_value(TeamBoardFollower {
                        mode: TeamBoardSubscriptionMode::Watch,
                        conversation_id,
                        identity,
                        author,
                    })?,
                ));
            }
        }
        writes.push((
            "message",
            record.message.id.clone(),
            serde_json::to_value(&record)?,
        ));
        Ok((record, writes))
    }

    async fn board_notifications(
        &self,
        message: &TeamMessage,
        conversation: &str,
        sender: &TeamBoardIdentity,
        mentions: &[TeamBoardMention],
    ) -> Result<Vec<TeamBoardNotification>> {
        let snapshot = self.store.board_snapshot().await?;
        let mut targets = BTreeMap::<String, TeamBoardNotification>::new();
        let mut add = |identity: TeamBoardIdentity,
                       reason: TeamBoardNotificationReason|
         -> Result<()> {
            if &identity != sender {
                let notification = targets.entry(identity_key(&identity)?).or_insert_with(|| {
                    TeamBoardNotification {
                        identity,
                        reasons: Vec::new(),
                    }
                });
                if !notification.reasons.contains(&reason) {
                    notification.reasons.push(reason);
                }
            }
            Ok(())
        };
        if message.reply_to.is_none() {
            for subscription in snapshot
                .topic_subscriptions
                .into_iter()
                .filter(|subscription| {
                    subscription.mode == TeamBoardSubscriptionMode::Wake
                        && Some(&subscription.topic) == message.topic.as_ref()
                })
            {
                add(
                    subscription.identity,
                    TeamBoardNotificationReason::TopicSubscription,
                )?;
            }
        } else {
            for follower in snapshot.followers.into_iter().filter(|follower| {
                follower.mode == TeamBoardSubscriptionMode::Wake
                    && follower.conversation_id == conversation
            }) {
                add(
                    follower.identity,
                    TeamBoardNotificationReason::ConversationFollow,
                )?;
            }
        }
        for mention in mentions {
            add(
                mention.identity.clone(),
                TeamBoardNotificationReason::Mention,
            )?;
        }
        let enrolled = self
            .agents()
            .await?
            .into_iter()
            .map(|agent| address_key(&agent.enrollment.address))
            .collect::<std::collections::HashSet<_>>();
        Ok(targets
            .into_values()
            .filter(|notification| match &notification.identity {
                TeamBoardIdentity::User => true,
                TeamBoardIdentity::Agent { address } => enrolled.contains(&address_key(address)),
            })
            .collect())
    }

    /// Keep the last confirmed name when an enrolled follower departs. Message
    /// attribution remains the immutable name captured when each entry arrived.
    pub(crate) async fn retain_departed_board_names(
        &self,
        departed: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let mut writes = Vec::new();
        for (key, follower) in self
            .store
            .list::<Option<TeamBoardFollower>>("board_follower", "", u32::MAX)
            .await?
        {
            let Some(mut follower) = follower else {
                continue;
            };
            let TeamBoardIdentity::Agent { address } = &follower.identity else {
                continue;
            };
            let address_key = address_key(address);
            if departed.contains(&address_key)
                && self
                    .store
                    .get::<Agent>("agent", &address_key)
                    .await?
                    .is_some()
            {
                follower.author = self.board_author(&follower.identity, false).await?;
                writes.push(("board_follower", key, serde_json::to_value(follower)?));
            }
        }
        if !writes.is_empty() {
            let mut clock = self.store.board_clock().await?;
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

fn requested_identity(
    actor: &TeamBoardIdentity,
    requested: Option<TeamBoardIdentity>,
) -> Result<TeamBoardIdentity> {
    let identity = requested.unwrap_or_else(|| actor.clone());
    ensure!(
        *actor == TeamBoardIdentity::User || identity == *actor,
        "agents can change only their own board state"
    );
    Ok(identity)
}

fn validate_mentions(body: &str, mentions: &[TeamBoardMention]) -> Result<()> {
    let mut end = 0;
    for mention in mentions {
        let start = mention.start as usize;
        let next_end = mention.end as usize;
        ensure!(
            start >= end
                && next_end > start
                && body.get(start..next_end) == Some(mention.display.as_str())
                && mention.display.starts_with('@'),
            "mention must match a distinct UTF-8 span including @"
        );
        end = next_end;
    }
    Ok(())
}
