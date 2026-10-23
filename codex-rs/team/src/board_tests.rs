use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::model::*;
use crate::tests::agent;
use crate::tests::manifest;

#[path = "board_navigation_tests.rs"]
mod navigation;

async fn post(coordinator: &Coordinator, body: &str) -> Result<String> {
    let response = coordinator
        .execute_operation(TeamOperation::new(TeamRequest::Board {
            request: TeamBoardParams::Post {
                topic: "review".into(),
                body: body.into(),
                mentions: Vec::new(),
            },
        }))
        .await?;
    let TeamResponse::Board {
        response:
            TeamBoardResponse::Mutation {
                conversation_id: Some(id),
                ..
            },
    } = response
    else {
        panic!("post mutation");
    };
    Ok(id)
}

async fn agent_board(
    coordinator: &Coordinator,
    sender: &TeamAddress,
    request: TeamBoardParams,
) -> Result<String> {
    let machine = crate::Machine {
        id: sender.machine_id.clone(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        environment_capabilities: Default::default(),
        id: sender.server_id.clone(),
        label: "Account".into(),
        endpoint: crate::Endpoint::Local {
            socket: coordinator.manifest.coordinator.state_dir.join("socket"),
        },
        cancellation_adapters: Vec::new(),
    };
    coordinator
        .store
        .put(
            "installation",
            &server_key(&machine.id, &server.id),
            &"installation-one",
        )
        .await?;
    let id = Uuid::now_v7().to_string();
    coordinator
        .accept_outbox(
            &machine,
            &server,
            &TeamOutboxEntry {
                sequence: 1,
                id: id.clone(),
                kind: "board".into(),
                payload: serde_json::json!({"sender":sender,"request":request}),
            },
        )
        .await?;
    assert!(
        coordinator
            .store
            .list::<RejectedOutbox>("rejected_outbox", "", u32::MAX)
            .await?
            .is_empty()
    );
    Ok(id)
}

#[tokio::test]
async fn board_replies_use_explicit_followers_and_mentions_without_auto_subscribing_agents()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let topic_only = agent(&coordinator, "Topic reader").await?;
    let follower = agent(&coordinator, "Reviewer").await?;
    let mentioned = agent(&coordinator, "Builder").await?;
    coordinator
        .subscribe(topic_only.clone(), "review".into(), true)
        .await?;
    let root = post(&coordinator, "Review this").await?;
    assert_eq!(
        coordinator
            .store
            .get::<MessageRecord>("message", &root)
            .await?
            .unwrap()
            .targets,
        Some(vec![topic_only.clone()])
    );
    agent_board(
        &coordinator,
        &follower,
        TeamBoardParams::Follow {
            conversation_id: root.clone(),
            identity: None,
            enabled: true,
        },
    )
    .await?;
    let reply_id = agent_board(
        &coordinator,
        &topic_only,
        TeamBoardParams::Reply {
            conversation_id: root.clone(),
            reply_to: None,
            body: "@Reviewer @Builder please check".into(),
            mentions: vec![
                TeamBoardMention {
                    identity: TeamBoardIdentity::Agent {
                        address: follower.clone(),
                    },
                    display: "@Reviewer".into(),
                    start: 0,
                    end: 9,
                },
                TeamBoardMention {
                    identity: TeamBoardIdentity::Agent {
                        address: mentioned.clone(),
                    },
                    display: "@Builder".into(),
                    start: 10,
                    end: 18,
                },
            ],
        },
    )
    .await?;
    let entry = coordinator
        .store
        .get::<TeamBoardEntry>("board_entry", &reply_id)
        .await?
        .unwrap();
    let mut targets = coordinator
        .store
        .get::<MessageRecord>("message", &reply_id)
        .await?
        .unwrap()
        .targets
        .unwrap();
    targets.sort_by_key(address_key);
    let mut expected = vec![follower.clone(), mentioned];
    expected.sort_by_key(address_key);
    assert_eq!(targets, expected);
    let reasons = entry
        .notifications
        .iter()
        .find(|notification| {
            notification.identity
                == TeamBoardIdentity::Agent {
                    address: follower.clone(),
                }
        })
        .unwrap()
        .reasons
        .clone();
    assert_eq!(
        reasons,
        vec![
            TeamBoardNotificationReason::ConversationFollow,
            TeamBoardNotificationReason::Mention
        ]
    );
    let snapshot = coordinator.board_snapshot().await?;
    let mut identities = snapshot
        .followers
        .iter()
        .map(|follower| follower.identity.clone())
        .collect::<Vec<_>>();
    identities.sort_by_key(|identity| serde_json::to_string(identity).unwrap());
    let mut expected = vec![
        TeamBoardIdentity::User,
        TeamBoardIdentity::Agent { address: follower },
    ];
    expected.sort_by_key(|identity| serde_json::to_string(identity).unwrap());
    assert_eq!(identities, expected);
    assert!(
        !entry
            .notifications
            .iter()
            .any(|notification| notification.identity
                == TeamBoardIdentity::Agent {
                    address: topic_only.clone()
                })
    );
    Ok(())
}

#[tokio::test]
async fn board_operation_retry_preserves_original_attribution_and_read_cursor_after_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let sender = agent(&coordinator, "Original name").await?;
    let root = agent_board(
        &coordinator,
        &sender,
        TeamBoardParams::Post {
            topic: "review".into(),
            body: "First".into(),
            mentions: Vec::new(),
        },
    )
    .await?;
    let operation = TeamOperation::new(TeamRequest::Board {
        request: TeamBoardParams::Reply {
            conversation_id: root.clone(),
            reply_to: None,
            body: "User response".into(),
            mentions: Vec::new(),
        },
    });
    let original_response =
        serde_json::to_value(coordinator.execute_operation(operation.clone()).await?)?;
    coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::MarkRead {
                conversation_id: root.clone(),
                identity: None,
                append_seq: 2,
            },
        })
        .await?;
    coordinator
        .execute(TeamRequest::Board {
            request: TeamBoardParams::MarkRead {
                conversation_id: root.clone(),
                identity: None,
                append_seq: 1,
            },
        })
        .await?;
    let key = address_key(&sender);
    let mut renamed = coordinator
        .store
        .get::<Agent>("agent", &key)
        .await?
        .unwrap();
    renamed.enrollment.alias = "Current name".into();
    coordinator.store.put("agent", &key, &renamed).await?;
    drop(coordinator);
    let coordinator = Coordinator::open(manifest).await?;
    assert_eq!(
        serde_json::to_value(coordinator.execute_operation(operation).await?)?,
        original_response
    );
    let snapshot = coordinator.board_snapshot().await?;
    assert_eq!(snapshot.entries.len(), 2);
    assert_eq!(snapshot.entries[0].author.name, "Original name");
    assert_eq!(
        snapshot
            .reads
            .iter()
            .map(|read| read.append_seq)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        snapshot
            .members
            .iter()
            .find(|member| member.author.identity
                == TeamBoardIdentity::Agent {
                    address: sender.clone()
                })
            .unwrap()
            .author
            .name,
        "Current name"
    );
    Ok(())
}

#[tokio::test]
async fn board_rejects_wrong_conversation_and_invalid_mention_without_partial_effects() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let first = post(&coordinator, "First").await?;
    let second = post(&coordinator, "Second").await?;
    let before = coordinator.board_snapshot().await?;
    for request in [
        TeamBoardParams::Reply {
            conversation_id: first,
            reply_to: Some(second),
            body: "Wrong parent".into(),
            mentions: Vec::new(),
        },
        TeamBoardParams::Post {
            topic: "review".into(),
            body: "é @user".into(),
            mentions: vec![TeamBoardMention {
                identity: TeamBoardIdentity::User,
                display: "@user".into(),
                start: 1,
                end: 6,
            }],
        },
    ] {
        assert!(
            coordinator
                .execute_operation(TeamOperation::new(TeamRequest::Board { request }))
                .await
                .is_err()
        );
    }
    assert_eq!(coordinator.board_snapshot().await?, before);
    Ok(())
}

#[tokio::test]
async fn board_backfill_retains_legacy_receipts_and_isolates_invalid_chains() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let mut root = coordinator
        .operator_message(
            None,
            "Old root".into(),
            TeamMessageKind::Board,
            None,
            Some("old".into()),
        )
        .await?;
    root.id = "ffffffff-ffff-4fff-8fff-ffffffffffff".into();
    root.created_at = 200;
    let mut child = root.clone();
    child.id = "00000000-0000-4000-8000-000000000001".into();
    child.created_at = 100;
    child.reply_to = Some(root.id.clone());
    let mut malformed = root.clone();
    malformed.id = Uuid::now_v7().to_string();
    malformed.reply_to = Some(malformed.id.clone());
    let mut originals = Vec::new();
    for message in [root.clone(), child.clone(), malformed.clone()] {
        let record = MessageRecord {
            message,
            delivery: TeamDeliveryMode::NextTurn,
            targets: Some(Vec::new()),
            receipts: Default::default(),
            published: true,
            error: None,
            external_url: None,
        };
        coordinator
            .store
            .put("message", &record.message.id, &record)
            .await?;
        originals.push(serde_json::to_value(record)?);
    }
    drop(coordinator);
    let coordinator = Coordinator::open(manifest).await?;
    let snapshot = coordinator.board_snapshot().await?;
    assert_eq!(snapshot.entries.len(), 3);
    let child_entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.message.id == child.id)
        .unwrap();
    let root_entry = snapshot
        .entries
        .iter()
        .find(|entry| entry.message.id == root.id)
        .unwrap();
    assert_eq!(child_entry.conversation_id, root.id);
    assert!(root_entry.append_seq < child_entry.append_seq);
    assert_eq!(
        snapshot
            .entries
            .iter()
            .find(|entry| entry.message.id == malformed.id)
            .unwrap()
            .conversation_id,
        malformed.id
    );
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| entry.author.legacy && entry.notifications.is_empty())
    );
    for original in originals {
        let id = original["message"]["id"].as_str().unwrap();
        assert_eq!(
            serde_json::to_value(
                coordinator
                    .store
                    .get::<MessageRecord>("message", id)
                    .await?
                    .unwrap()
            )?,
            original
        );
    }
    Ok(())
}

#[tokio::test]
async fn board_snapshot_is_coherent_during_concurrent_commits() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let initial_revision = coordinator.board_snapshot().await?.revision;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let writer = coordinator.clone();
    let writer_barrier = barrier.clone();
    let writes = tokio::spawn(async move {
        writer_barrier.wait().await;
        for index in 0..16 {
            post(&writer, &format!("Concurrent post {index}")).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    barrier.wait().await;
    for _ in 0..16 {
        let snapshot = coordinator.board_snapshot().await?;
        assert_eq!(
            snapshot.revision,
            initial_revision + snapshot.entries.len() as i64
        );
        assert_eq!(snapshot.followers.len(), snapshot.entries.len());
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.append_seq)
                .collect::<Vec<_>>(),
            (1..=snapshot.entries.len() as i64).collect::<Vec<_>>()
        );
        assert!(snapshot.followers.iter().all(|follower| {
            snapshot
                .entries
                .iter()
                .any(|entry| entry.conversation_id == follower.conversation_id)
        }));
    }
    writes.await??;
    assert_eq!(coordinator.board_snapshot().await?.entries.len(), 16);
    Ok(())
}

#[tokio::test]
async fn board_followers_show_current_and_last_known_names_without_changing_authors() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let sender = agent(&coordinator, "Original name").await?;
    let root = agent_board(
        &coordinator,
        &sender,
        TeamBoardParams::Post {
            topic: "review".into(),
            body: "First".into(),
            mentions: Vec::new(),
        },
    )
    .await?;
    agent_board(
        &coordinator,
        &sender,
        TeamBoardParams::Follow {
            conversation_id: root,
            identity: None,
            enabled: true,
        },
    )
    .await?;
    let key = address_key(&sender);
    let mut renamed = coordinator
        .store
        .get::<Agent>("agent", &key)
        .await?
        .unwrap();
    renamed.enrollment.alias = "Latest confirmed name".into();
    coordinator.store.put("agent", &key, &renamed).await?;
    let live = coordinator.board_snapshot().await?;
    assert_eq!(live.followers[0].author.name, "Latest confirmed name");
    assert_eq!(live.entries[0].author.name, "Original name");
    coordinator
        .forget_agents(&std::collections::HashSet::from([key]))
        .await?;
    let departed = coordinator.board_snapshot().await?;
    assert_eq!(departed.followers[0].author.name, "Latest confirmed name");
    assert_eq!(departed.entries[0].author.name, "Original name");
    assert_eq!(departed.members.len(), 1);
    Ok(())
}

#[tokio::test]
async fn board_agent_cannot_follow_another_identity_and_rejection_is_durable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let sender = agent(&coordinator, "Reviewer").await?;
    let other = agent(&coordinator, "Builder").await?;
    let root = post(&coordinator, "Review").await?;
    let before = coordinator.board_snapshot().await?;
    let machine = crate::Machine {
        id: sender.machine_id.clone(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        environment_capabilities: Default::default(),
        id: sender.server_id.clone(),
        label: "Account".into(),
        endpoint: crate::Endpoint::Local {
            socket: directory.path().join("socket"),
        },
        cancellation_adapters: Vec::new(),
    };
    coordinator
        .store
        .put(
            "installation",
            &server_key(&machine.id, &server.id),
            &"installation-one",
        )
        .await?;
    let entry = TeamOutboxEntry {
        sequence: 1,
        id: Uuid::now_v7().to_string(),
        kind: "board".into(),
        payload: serde_json::json!({
            "sender":sender,
            "request": TeamBoardParams::Follow { conversation_id: root, identity: Some(TeamBoardIdentity::Agent { address: other }), enabled: true },
        }),
    };
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    assert_eq!(coordinator.board_snapshot().await?, before);
    let rejected = coordinator
        .store
        .list::<RejectedOutbox>("rejected_outbox", "", u32::MAX)
        .await?;
    assert_eq!(rejected.len(), 1);
    assert_eq!(
        rejected[0].1.reason,
        "agents can change only their own board state"
    );
    Ok(())
}

#[path = "board_subscription_tests.rs"]
mod subscriptions;
