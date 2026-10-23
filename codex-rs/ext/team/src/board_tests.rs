use std::sync::Arc;
use std::sync::Weak;

use anyhow::Result;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use crate::PreparedTeamHost;
use crate::TeamHostService;

async fn fixture() -> Result<(
    tempfile::TempDir,
    Arc<TeamHostService>,
    TeamAddress,
    TeamBoardSnapshot,
)> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let service = TeamHostService::new(
        PreparedTeamHost::load(&config, "installation".into()).await?,
        Weak::new(),
    );
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "primary".into(),
        thread_id: ThreadId::new().to_string(),
    };
    service
        .enable()
        .await?
        .enroll(
            &TeamEnrollment {
                address: address.clone(),
                alias: "Reviewer".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            "credential",
        )
        .await?;
    service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 1,
                mode: TeamExecutionMode::Paused,
            },
        })
        .await?;
    let message = TeamMessage {
        id: "conversation".into(),
        sender: address.clone(),
        recipient: None,
        kind: TeamMessageKind::Board,
        body: "Please review this".into(),
        reply_to: None,
        topic: Some("general".into()),
        created_at: 1,
    };
    let snapshot = TeamBoardSnapshot {
        revision: 10,
        entries: vec![TeamBoardEntry {
            conversation_id: message.id.clone(),
            message,
            append_seq: 3,
            author: TeamBoardAuthor {
                identity: TeamBoardIdentity::User,
                name: "User".into(),
                machine_name: None,
                legacy: false,
            },
            mentions: Vec::new(),
            notifications: Vec::new(),
        }],
        ..Default::default()
    };
    sync(&service, snapshot.clone()).await?;
    Ok((home, service, address, snapshot))
}

async fn sync(service: &TeamHostService, board_state: TeamBoardSnapshot) -> Result<()> {
    service
        .sync(TeamSyncParams {
            administration: Vec::new(),
            retirements: Vec::new(),
            board_purges: Vec::new(),
            message_status: Default::default(),
            coordinator_id: "coordinator".into(),
            directory: Default::default(),
            board: Vec::new(),
            cancellation_adapters: Vec::new(),
            board_state,
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn purge_sync_suppresses_pending_mail_and_filters_stale_cache_and_delivery() -> Result<()> {
    let (_home, service, address, snapshot) = fixture().await?;
    let store = service.enable().await?;
    let message = snapshot.entries[0].message.clone();
    store.put("board:general", &message.id, &message).await?;
    let delivery = TeamDelivery {
        id: "pending-purge-delivery".into(),
        thread_id: address.thread_id.clone(),
        message: message.clone(),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
    };
    assert_eq!(
        service
            .deliver(TeamDeliverParams {
                coordinator_id: "coordinator".into(),
                delivery: delivery.clone(),
            })
            .await?
            .receipt
            .state,
        TeamReceiptState::Pending
    );
    let mut removed_snapshot = snapshot.clone();
    removed_snapshot.revision += 1;
    removed_snapshot.entries.clear();
    let response = service
        .sync(TeamSyncParams {
            administration: Vec::new(),
            retirements: Vec::new(),
            coordinator_id: "coordinator".into(),
            directory: Default::default(),
            board: Vec::new(),
            board_state: removed_snapshot,
            cancellation_adapters: Vec::new(),
            message_status: Default::default(),
            board_purges: vec![TeamBoardPurgeJournal {
                operation_id: "purge".into(),
                revision: snapshot.revision + 1,
                conversation_ids: vec![message.id.clone()],
                message_ids: vec![message.id.clone()],
            }],
        })
        .await?;
    assert_eq!(response.board_purge_acknowledgements, vec!["purge"]);
    assert_eq!(
        store.deliveries(&address.thread_id).await?[0].1.state,
        TeamReceiptState::Rejected
    );
    assert!(
        store
            .get::<TeamMessage>("board:general", &message.id)
            .await?
            .is_none()
    );

    // An old sender can race purge acknowledgement. Higher transport/snapshot
    // revisions do not authorize resurrecting the removed content.
    let mut stale = snapshot;
    stale.revision += 2;
    service
        .sync(TeamSyncParams {
            administration: Vec::new(),
            retirements: Vec::new(),
            coordinator_id: "coordinator".into(),
            directory: Default::default(),
            board: vec![message.clone()],
            board_state: stale,
            cancellation_adapters: Vec::new(),
            message_status: Default::default(),
            board_purges: Vec::new(),
        })
        .await?;
    assert!(
        store
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?
            .unwrap()
            .entries
            .is_empty()
    );
    let receipt = service
        .deliver(TeamDeliverParams {
            coordinator_id: "coordinator".into(),
            delivery,
        })
        .await?
        .receipt;
    assert_eq!(receipt.state, TeamReceiptState::Rejected);
    assert_eq!(receipt.detail.as_deref(), Some("Removed before delivery."));
    assert!(
        store.deliveries(&address.thread_id).await?[0]
            .0
            .message
            .body
            .is_empty()
    );
    Ok(())
}

#[test]
fn purge_is_not_an_agent_board_action() {
    let action =
        serde_json::json!({"type":"boardAction","request":{"type":"purge","scope":{"type":"all"}}});
    assert!(serde_json::from_value::<TeamAgentActionParams>(action).is_err());
}

fn list() -> TeamAgentActionParams {
    TeamAgentActionParams::BoardAction {
        request: TeamBoardParams::List {
            filter: TeamBoardFilter::All,
            query: None,
            topic: None,
            cursor: None,
            limit: None,
            identity: None,
        },
    }
}

#[tokio::test]
async fn native_and_credential_tools_share_committed_board_and_queued_follow_receipt() -> Result<()>
{
    let (_home, service, address, mut snapshot) = fixture().await?;
    let thread_id = ThreadId::from_string(&address.thread_id)?;
    let credential = service
        .agent_credential(TeamAgentCredentialParams {
            thread_id: address.thread_id.clone(),
        })
        .await?
        .credential;
    let native = service.agent_action(thread_id, list(), None).await?;
    let bridge = service
        .agent_request(TeamAgentRequestParams {
            thread_id: address.thread_id.clone(),
            credential,
            operation_id: None,
            action: list(),
        })
        .await?;
    assert_eq!(native, bridge);
    assert_eq!(native.result["revision"], 10);
    assert_eq!(native.result["data"][0]["subscriptionMode"], "off");
    assert_eq!(native.result["type"], "threads");
    assert_eq!(native.result["data"][0]["unreadCount"], 1);

    let action = TeamAgentActionParams::BoardAction {
        request: TeamBoardParams::Follow {
            conversation_id: "conversation".into(),
            identity: None,
            enabled: true,
        },
    };
    let queued = service
        .agent_action(thread_id, action.clone(), Some("follow-once".into()))
        .await?;
    assert_eq!(
        service
            .agent_action(thread_id, action, Some("follow-once".into()))
            .await?,
        queued
    );
    assert_eq!(queued.result["state"], "queued");
    let store = service.store.get().unwrap();
    let outbox = store.outbox(0, 100).await?;
    let board = outbox
        .iter()
        .filter(|entry| entry.kind == "board")
        .collect::<Vec<_>>();
    assert_eq!(board.len(), 1);
    assert_eq!(
        board[0].payload,
        serde_json::json!({
            "sender": address,
            "request": { "type": "follow", "conversationId": "conversation", "identity": null, "enabled": true }
        })
    );
    assert_eq!(service.agent_action(thread_id, list(), None).await?, native);

    // Only a later committed coordinator revision confirms the subscription.
    snapshot.revision = 11;
    let identity = TeamBoardIdentity::Agent {
        address: address.clone(),
    };
    snapshot.followers.push(TeamBoardFollower {
        mode: TeamBoardSubscriptionMode::Wake,
        conversation_id: "conversation".into(),
        identity: identity.clone(),
        author: TeamBoardAuthor {
            identity,
            name: "Reviewer".into(),
            machine_name: Some("Machine".into()),
            legacy: false,
        },
    });
    sync(&service, snapshot).await?;
    let confirmed = service.agent_action(thread_id, list(), None).await?;
    assert_eq!(confirmed.result["revision"], 11);
    assert_eq!(confirmed.result["data"][0]["subscriptionMode"], "wake");
    Ok(())
}

#[tokio::test]
async fn board_identity_override_cannot_follow_or_read_as_the_operator() -> Result<()> {
    let (_home, service, address, _) = fixture().await?;
    let thread_id = ThreadId::from_string(&address.thread_id)?;
    for request in [
        TeamBoardParams::Follow {
            conversation_id: "conversation".into(),
            identity: Some(TeamBoardIdentity::User),
            enabled: true,
        },
        TeamBoardParams::Read {
            conversation_id: "conversation".into(),
            cursor: None,
            limit: None,
            identity: Some(TeamBoardIdentity::User),
        },
        TeamBoardParams::MarkRead {
            conversation_id: "conversation".into(),
            identity: Some(TeamBoardIdentity::User),
            append_seq: 3,
        },
        TeamBoardParams::TopicOverview {
            cursor: None,
            limit: None,
            identity: Some(TeamBoardIdentity::User),
        },
        TeamBoardParams::SubscriptionSet {
            target: TeamBoardSubscriptionTarget::Topic {
                topic: "review".into(),
            },
            identity: Some(TeamBoardIdentity::User),
            mode: TeamBoardSubscriptionMode::Watch,
        },
    ] {
        let error = service
            .agent_action(
                thread_id,
                TeamAgentActionParams::BoardAction { request },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "agent board actions may only use their own identity"
        );
    }
    assert!(
        service
            .store
            .get()
            .unwrap()
            .outbox(0, 100)
            .await?
            .iter()
            .all(|entry| entry.kind != "board")
    );
    Ok(())
}

#[tokio::test]
async fn board_cache_never_regresses_after_reordered_sync() -> Result<()> {
    let (_home, service, _, mut snapshot) = fixture().await?;
    let expected = snapshot.clone();
    snapshot.revision = 9;
    snapshot.entries.clear();
    assert_eq!(
        sync(&service, snapshot).await.unwrap_err().to_string(),
        "board snapshot revision must not regress"
    );
    assert_eq!(
        service
            .store
            .get()
            .unwrap()
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?,
        Some(expected)
    );
    Ok(())
}

#[tokio::test]
async fn native_board_tool_schema_is_supported_by_the_model_tool_parser() -> Result<()> {
    codex_extension_api::parse_tool_input_schema_without_compaction(&team_agent_tool_schema())?;
    Ok(())
}

#[tokio::test]
async fn descendant_board_mutations_keep_the_bound_child_identity() -> Result<()> {
    let (_home, service, root_address, _) = fixture().await?;
    let child = ThreadId::new();
    service
        .gate
        .inherit(child, ThreadId::from_string(&root_address.thread_id)?);
    service
        .store
        .get()
        .unwrap()
        .put("member_credential", &child.to_string(), &"child-credential")
        .await?;
    let action = TeamAgentActionParams::BoardAction {
        request: TeamBoardParams::Post {
            topic: "general".into(),
            body: "A child report".into(),
            mentions: Vec::new(),
        },
    };
    let native = service
        .agent_action(child, action.clone(), Some("child-post".into()))
        .await?;
    let bridge = service
        .agent_request(TeamAgentRequestParams {
            thread_id: child.to_string(),
            credential: "child-credential".into(),
            operation_id: Some("child-post".into()),
            action,
        })
        .await?;
    assert_eq!(native, bridge);
    let entries = service
        .store
        .get()
        .unwrap()
        .outbox(0, 100)
        .await?
        .into_iter()
        .filter(|entry| entry.kind == "board")
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].payload["sender"],
        serde_json::to_value(TeamAddress {
            thread_id: child.to_string(),
            ..root_address
        })?
    );
    Ok(())
}

#[tokio::test]
async fn malformed_unicode_mention_never_enters_the_durable_outbox() -> Result<()> {
    let (_home, service, address, _) = fixture().await?;
    let error = service
        .agent_action(
            ThreadId::from_string(&address.thread_id)?,
            TeamAgentActionParams::BoardAction {
                request: TeamBoardParams::Post {
                    topic: "general".into(),
                    body: "é @Reviewer".into(),
                    mentions: vec![TeamBoardMention {
                        identity: TeamBoardIdentity::Agent { address },
                        display: "@Reviewer".into(),
                        start: 2,
                        end: 11,
                    }],
                },
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "mention range must match its displayed @name in the body"
    );
    assert!(
        service
            .store
            .get()
            .unwrap()
            .outbox(0, 100)
            .await?
            .iter()
            .all(|entry| entry.kind != "board")
    );
    Ok(())
}

#[path = "directory_cache_tests.rs"]
mod directory_cache_tests;

#[tokio::test]
async fn unread_pages_queue_ordered_acknowledgements_before_the_next_sync() -> Result<()> {
    let (_home, service, address, mut snapshot) = fixture().await?;
    let thread_id = ThreadId::from_string(&address.thread_id)?;
    let mut reply = snapshot.entries[0].clone();
    reply.message.id = "reply".into();
    reply.message.reply_to = Some("conversation".into());
    reply.message.body = "Later reply".into();
    reply.append_seq = 8;
    snapshot.entries.push(reply);
    snapshot.revision += 1;
    sync(&service, snapshot.clone()).await?;
    let read = |cursor| TeamAgentActionParams::BoardAction {
        request: TeamBoardParams::ReadPage {
            conversation_id: "conversation".into(),
            mode: TeamBoardReadMode::Unread,
            anchor_message_id: None,
            cursor,
            limit: Some(1),
            identity: None,
        },
    };
    let first = service
        .agent_action(thread_id, read(None), None)
        .await?
        .result;
    let next_cursor = first["page"]["nextCursor"]
        .as_str()
        .expect("first page cursor")
        .to_owned();
    let second = service
        .agent_action(thread_id, read(Some(next_cursor)), None)
        .await?
        .result;
    assert_eq!(first["page"]["data"][0]["id"], "conversation");
    assert_eq!(second["page"]["data"][0]["id"], "reply");
    let store = service.store.get().expect("store");
    assert!(store.outbox(0, 100).await?.is_empty());

    for (operation, page) in [("ack-first", first), ("ack-second", second)] {
        let action = TeamAgentActionParams::BoardAction {
            request: TeamBoardParams::Acknowledge {
                token: page["page"]["acknowledgementToken"]
                    .as_str()
                    .expect("page token")
                    .into(),
            },
        };
        let queued = service
            .agent_action(thread_id, action.clone(), Some(operation.into()))
            .await?;
        assert_eq!(queued.result["state"], "queued");
        assert_eq!(queued.result["route"]["conversationId"], "conversation");
        assert_eq!(
            service
                .agent_action(thread_id, action, Some(operation.into()))
                .await?,
            queued
        );
    }
    let outbox = store.outbox(0, 100).await?;
    assert_eq!(outbox.len(), 2);
    let acknowledged = outbox
        .iter()
        .map(|entry| {
            let token = entry.payload["request"]["token"]
                .as_str()
                .expect("ack token");
            serde_json::from_str::<serde_json::Value>(token).map(|token| token["through"].clone())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        acknowledged,
        vec![serde_json::json!(3), serde_json::json!(8)]
    );
    // Fetching and queueing do not mutate the synchronized read marker.
    assert_eq!(
        store
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?,
        Some(snapshot)
    );
    Ok(())
}
