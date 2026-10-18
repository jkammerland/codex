//! Subscription mutations share the Board transaction and durable operation receipt.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::board::BoardWrites;
use crate::board::subscription_key;
use crate::coordinator::Coordinator;
use crate::routing::Subscription;

pub(crate) enum SubscriptionChange {
    Legacy(bool),
    Explicit(TeamBoardSubscriptionMode),
}

impl SubscriptionChange {
    fn mode(
        self,
        identity: &TeamBoardIdentity,
        current: Option<TeamBoardSubscriptionMode>,
    ) -> Result<TeamBoardSubscriptionMode> {
        let mode = match self {
            Self::Legacy(false) => TeamBoardSubscriptionMode::Off,
            Self::Legacy(true) => current.unwrap_or(match identity {
                TeamBoardIdentity::User => TeamBoardSubscriptionMode::Watch,
                TeamBoardIdentity::Agent { .. } => TeamBoardSubscriptionMode::Wake,
            }),
            Self::Explicit(mode) => mode,
        };
        ensure!(
            *identity != TeamBoardIdentity::User || mode != TeamBoardSubscriptionMode::Wake,
            "human subscriptions support Off or Watch; Wake requires an agent"
        );
        Ok(mode)
    }
}

impl Coordinator {
    pub(crate) async fn prepare_board_subscription(
        &self,
        target: TeamBoardSubscriptionTarget,
        identity: TeamBoardIdentity,
        change: SubscriptionChange,
    ) -> Result<BoardWrites> {
        match target {
            TeamBoardSubscriptionTarget::Conversation { conversation_id } => {
                self.require_conversation(&conversation_id).await?;
                let key = subscription_key(&conversation_id, &identity)?;
                let current = self
                    .store
                    .get::<Option<TeamBoardFollower>>("board_follower", &key)
                    .await?
                    .flatten();
                let mode = change.mode(&identity, current.map(|follower| follower.mode))?;
                let follower = if mode == TeamBoardSubscriptionMode::Off {
                    None
                } else {
                    Some(TeamBoardFollower {
                        mode,
                        conversation_id,
                        author: self.board_author(&identity, false).await?,
                        identity,
                    })
                };
                Ok(vec![(
                    "board_follower",
                    key,
                    serde_json::to_value(follower)?,
                )])
            }
            TeamBoardSubscriptionTarget::Topic { topic } => {
                ensure!(!topic.trim().is_empty(), "board topic is required");
                let key = subscription_key(&topic, &identity)?;
                let current = self
                    .store
                    .get::<Option<TeamBoardTopicSubscription>>("board_topic", &key)
                    .await?
                    .flatten();
                let mode = change.mode(&identity, current.map(|subscription| subscription.mode))?;
                let mut writes = Vec::new();
                if mode != TeamBoardSubscriptionMode::Off {
                    self.board_author(&identity, false).await?;
                }
                if let TeamBoardIdentity::Agent { address } = &identity {
                    // The legacy routing mirror must exclude Watch entirely, so
                    // a later ordinary turn cannot accidentally drain its posts.
                    // Removal remains possible after a participant has left.
                    let (legacy_key, legacy) = if mode == TeamBoardSubscriptionMode::Off {
                        (
                            serde_json::to_string(&(&topic, address))?,
                            Subscription {
                                sender: address.clone(),
                                topic: topic.clone(),
                                enabled: false,
                            },
                        )
                    } else {
                        self.prepare_subscription(
                            address.clone(),
                            topic.clone(),
                            mode == TeamBoardSubscriptionMode::Wake,
                        )
                        .await?
                    };
                    writes.push(("subscription", legacy_key, serde_json::to_value(legacy)?));
                }
                let subscription = (mode != TeamBoardSubscriptionMode::Off).then_some(
                    TeamBoardTopicSubscription {
                        mode,
                        topic,
                        identity,
                    },
                );
                writes.push(("board_topic", key, serde_json::to_value(subscription)?));
                Ok(writes)
            }
        }
    }

    /// Legacy topic APIs preserve explicitly selected modes, including Watch.
    pub(crate) async fn subscription_effects(
        &self,
        _key: String,
        subscription: Subscription,
    ) -> Result<BoardWrites> {
        let mut writes = self
            .prepare_board_subscription(
                TeamBoardSubscriptionTarget::Topic {
                    topic: subscription.topic,
                },
                TeamBoardIdentity::Agent {
                    address: subscription.sender,
                },
                SubscriptionChange::Legacy(subscription.enabled),
            )
            .await?;
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
        Ok(writes)
    }
}
