use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::agent_board;
use super::post;
use crate::coordinator::Coordinator;
use crate::model::*;
use crate::routing::Subscription;
use crate::tests::agent;
use crate::tests::manifest;

async fn set_mode(
    coordinator: &Coordinator,
    address: &TeamAddress,
    target: TeamBoardSubscriptionTarget,
    mode: TeamBoardSubscriptionMode,
) -> Result<()> {
    agent_board(
        coordinator,
        address,
        TeamBoardParams::SubscriptionSet {
            target,
            identity: None,
            mode,
        },
    )
    .await?;
    Ok(())
}

async fn targets(coordinator: &Coordinator, id: &str) -> Result<Vec<TeamAddress>> {
    Ok(coordinator
        .store
        .get::<MessageRecord>("message", id)
        .await?
        .unwrap()
        .targets
        .unwrap())
}

#[tokio::test]
async fn watch_never_routes_but_mentions_deliver_and_wake_is_future_only() -> Result<()> {
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(home.path())).await?;
    let watcher = agent(&coordinator, "Watcher").await?;
    let author = agent(&coordinator, "Author").await?;
    let topic = TeamBoardSubscriptionTarget::Topic {
        topic: "review".into(),
    };
    set_mode(
        &coordinator,
        &watcher,
        topic.clone(),
        TeamBoardSubscriptionMode::Watch,
    )
    .await?;
    let watched_root = post(&coordinator, "Passive topic root").await?;
    assert_eq!(
        targets(&coordinator, &watched_root).await?,
        Vec::<TeamAddress>::new()
    );
    set_mode(
        &coordinator,
        &watcher,
        TeamBoardSubscriptionTarget::Conversation {
            conversation_id: watched_root.clone(),
        },
        TeamBoardSubscriptionMode::Watch,
    )
    .await?;
    let watched_reply = agent_board(
        &coordinator,
        &author,
        TeamBoardParams::Reply {
            conversation_id: watched_root.clone(),
            reply_to: None,
            body: "Passive reply".into(),
            mentions: Vec::new(),
        },
    )
    .await?;
    assert_eq!(
        targets(&coordinator, &watched_reply).await?,
        Vec::<TeamAddress>::new()
    );
    let mention = agent_board(
        &coordinator,
        &author,
        TeamBoardParams::Reply {
            conversation_id: watched_root.clone(),
            reply_to: None,
            body: "@Watcher explicit mention".into(),
            mentions: vec![TeamBoardMention {
                identity: TeamBoardIdentity::Agent {
                    address: watcher.clone(),
                },
                display: "@Watcher".into(),
                start: 0,
                end: 8,
            }],
        },
    )
    .await?;
    assert_eq!(
        targets(&coordinator, &mention).await?,
        vec![watcher.clone()]
    );
    let entry = coordinator
        .store
        .get::<TeamBoardEntry>("board_entry", &mention)
        .await?
        .unwrap();
    assert_eq!(
        entry.notifications[0].reasons,
        vec![TeamBoardNotificationReason::Mention]
    );

    // Both old enable entry points must retain an explicitly passive selection.
    coordinator
        .subscribe(watcher.clone(), "review".into(), true)
        .await?;
    agent_board(
        &coordinator,
        &watcher,
        TeamBoardParams::Follow {
            conversation_id: watched_root.clone(),
            identity: None,
            enabled: true,
        },
    )
    .await?;
    let subscriptions = coordinator
        .store
        .list::<Subscription>("subscription", "", u32::MAX)
        .await?;
    assert_eq!(subscriptions.len(), 1);
    assert!(!subscriptions[0].1.enabled);
    let snapshot = coordinator.board_snapshot().await?;
    assert_eq!(
        snapshot.topic_subscriptions[0].mode,
        TeamBoardSubscriptionMode::Watch
    );
    assert_eq!(
        snapshot
            .followers
            .iter()
            .find(|follower| follower.identity
                == TeamBoardIdentity::Agent {
                    address: watcher.clone()
                })
            .unwrap()
            .mode,
        TeamBoardSubscriptionMode::Watch
    );

    set_mode(
        &coordinator,
        &watcher,
        topic.clone(),
        TeamBoardSubscriptionMode::Wake,
    )
    .await?;
    let waking_root = post(&coordinator, "Wake root").await?;
    assert_eq!(
        targets(&coordinator, &waking_root).await?,
        vec![watcher.clone()]
    );
    let reply = agent_board(
        &coordinator,
        &author,
        TeamBoardParams::Reply {
            conversation_id: waking_root.clone(),
            reply_to: None,
            body: "Topic wake excludes replies".into(),
            mentions: Vec::new(),
        },
    )
    .await?;
    assert!(targets(&coordinator, &reply).await?.is_empty());
    let own = agent_board(
        &coordinator,
        &watcher,
        TeamBoardParams::Post {
            topic: "review".into(),
            body: "No self notification".into(),
            mentions: Vec::new(),
        },
    )
    .await?;
    assert!(targets(&coordinator, &own).await?.is_empty());
    set_mode(
        &coordinator,
        &watcher,
        topic,
        TeamBoardSubscriptionMode::Watch,
    )
    .await?;
    let after = post(&coordinator, "Passive again").await?;
    let legacy_message = coordinator
        .operator_message(
            None,
            "Legacy unpublished root".into(),
            TeamMessageKind::Board,
            None,
            Some("review".into()),
        )
        .await?;
    let legacy = coordinator
        .prepare_message(legacy_message, TeamDeliveryMode::NextTurn)
        .await?;
    let legacy_id = legacy.message.id.clone();
    coordinator
        .store
        .put("message", &legacy_id, &legacy)
        .await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        })
        .await?;
    // Publication and later ordinary turn admission can only consume frozen
    // targets. A mode change neither retracts them nor backfills old posts.
    coordinator.publish().await?;
    assert!(targets(&coordinator, &watched_root).await?.is_empty());
    assert!(targets(&coordinator, &watched_reply).await?.is_empty());
    assert!(targets(&coordinator, &after).await?.is_empty());
    assert!(targets(&coordinator, &legacy_id).await?.is_empty());
    assert_eq!(targets(&coordinator, &waking_root).await?, vec![watcher]);
    Ok(())
}

#[tokio::test]
async fn subscription_retry_after_restart_returns_original_receipt_without_reapplying() -> Result<()>
{
    let home = tempfile::tempdir()?;
    let manifest = manifest(home.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let subscriber = agent(&coordinator, "Subscriber").await?;
    let target = TeamBoardSubscriptionTarget::Topic {
        topic: "review".into(),
    };
    let operation = TeamOperation::new(TeamRequest::Board {
        request: TeamBoardParams::SubscriptionSet {
            target: target.clone(),
            identity: Some(TeamBoardIdentity::Agent {
                address: subscriber.clone(),
            }),
            mode: TeamBoardSubscriptionMode::Watch,
        },
    });
    let first = serde_json::to_value(coordinator.execute_operation(operation.clone()).await?)?;
    set_mode(
        &coordinator,
        &subscriber,
        target,
        TeamBoardSubscriptionMode::Off,
    )
    .await?;
    let confirmed = coordinator.board_snapshot().await?;
    drop(coordinator);
    let coordinator = Coordinator::open(manifest).await?;
    assert_eq!(
        serde_json::to_value(coordinator.execute_operation(operation).await?)?,
        first
    );
    assert_eq!(coordinator.board_snapshot().await?, confirmed);
    assert!(confirmed.topic_subscriptions.is_empty());
    Ok(())
}

#[tokio::test]
async fn human_wake_is_rejected_without_advancing_board_state() -> Result<()> {
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(home.path())).await?;
    let before = coordinator.board_snapshot().await?;
    let error = coordinator
        .execute_operation(TeamOperation::new(TeamRequest::Board {
            request: TeamBoardParams::SubscriptionSet {
                target: TeamBoardSubscriptionTarget::Topic {
                    topic: "review".into(),
                },
                identity: None,
                mode: TeamBoardSubscriptionMode::Wake,
            },
        }))
        .await
        .expect_err("human cannot wake");
    assert!(
        error
            .to_string()
            .contains("human subscriptions support Off or Watch")
    );
    assert_eq!(coordinator.board_snapshot().await?, before);
    Ok(())
}

#[tokio::test]
async fn operator_can_remove_subscriptions_after_participant_departure() -> Result<()> {
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(home.path())).await?;
    let subscriber = agent(&coordinator, "Departing").await?;
    let root = post(&coordinator, "Context").await?;
    let targets = [
        TeamBoardSubscriptionTarget::Topic {
            topic: "review".into(),
        },
        TeamBoardSubscriptionTarget::Conversation {
            conversation_id: root,
        },
    ];
    for target in &targets {
        set_mode(
            &coordinator,
            &subscriber,
            target.clone(),
            TeamBoardSubscriptionMode::Watch,
        )
        .await?;
    }
    coordinator
        .store
        .delete("agent", &address_key(&subscriber))
        .await?;
    for target in targets {
        coordinator
            .execute_operation(TeamOperation::new(TeamRequest::Board {
                request: TeamBoardParams::SubscriptionSet {
                    target,
                    identity: Some(TeamBoardIdentity::Agent {
                        address: subscriber.clone(),
                    }),
                    mode: TeamBoardSubscriptionMode::Off,
                },
            }))
            .await?;
    }
    let snapshot = coordinator.board_snapshot().await?;
    assert!(snapshot.topic_subscriptions.is_empty());
    assert!(
        snapshot
            .followers
            .iter()
            .all(|follower| follower.identity == TeamBoardIdentity::User)
    );
    assert!(
        coordinator
            .store
            .list::<Subscription>("subscription", "", u32::MAX)
            .await?
            .iter()
            .all(|(_, subscription)| !subscription.enabled)
    );
    Ok(())
}
