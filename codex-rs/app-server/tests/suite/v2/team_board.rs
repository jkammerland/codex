use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn board_delivery_waits_for_synced_named_conversation_context() -> Result<()> {
    let server =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "Review acknowledged",
        )?])
        .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = TeamAddress {
        machine_id: "remote".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let control = TeamControlState {
        coordinator_id: "board-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    let enrollment: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "Reviewer".into(),
                control: control.clone(),
            },
        })
        .await?;
    let message = TeamMessage {
        id: "board-message".into(),
        sender: TeamAddress {
            machine_id: "operator".into(),
            server_id: "operator".into(),
            thread_id: "operator".into(),
        },
        recipient: None,
        kind: TeamMessageKind::Board,
        body: "@Reviewer please review this".into(),
        reply_to: None,
        topic: Some("review".into()),
        created_at: 42,
    };
    let delivery = TeamDelivery {
        id: "board-delivery".into(),
        thread_id: thread.id.clone(),
        message: message.clone(),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
    };
    let initial: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: control.coordinator_id.clone(),
                delivery: delivery.clone(),
            },
        })
        .await?;
    assert_eq!(initial.receipt.state, TeamReceiptState::Pending);

    // Observe the durable pending reason through change events, rather than
    // relying on elapsed time to guess whether a model call would have started.
    loop {
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
        let outbox: TeamOutboxListResponse = app
            .request(|request_id| ClientRequest::TeamOutboxList {
                request_id,
                params: TeamOutboxListParams {
                    coordinator_id: control.coordinator_id.clone(),
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        if outbox.data.iter().any(|entry| {
            entry.kind == "receipt"
                && entry.payload["deliveryId"] == delivery.id
                && entry.payload["detail"]
                    == "Waiting for Board conversation metadata to synchronize."
        }) {
            break;
        }
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );

    let identity = TeamBoardIdentity::Agent {
        address: address.clone(),
    };
    let _: TeamSyncResponse = app
        .request(|request_id| ClientRequest::TeamSync {
            request_id,
            params: TeamSyncParams {
                administration: Vec::new(),
                retirements: Vec::new(),
                board_purges: Vec::new(),
                message_status: Default::default(),
                coordinator_id: control.coordinator_id.clone(),
                directory: codex_app_server_protocol::TeamDirectorySnapshot {
                    revision: 1,
                    data: vec![codex_app_server_protocol::TeamDirectoryEntry {
                        address: enrollment.enrollment.address.clone(),
                        alias: enrollment.enrollment.alias.clone(),
                        manually_held: false,
                        continuation_turn_id: None,
                        discovery: Default::default(),
                        current_model: None,
                        current_effort: None,
                        next_model: None,
                        next_effort: None,
                    }],
                },
                board: vec![message.clone()],
                cancellation_adapters: Vec::new(),
                board_state: TeamBoardSnapshot {
                    revision: 7,
                    entries: vec![TeamBoardEntry {
                        message: message.clone(),
                        conversation_id: message.id.clone(),
                        append_seq: 5,
                        author: TeamBoardAuthor {
                            identity: TeamBoardIdentity::User,
                            name: "User".into(),
                            machine_name: None,
                            legacy: false,
                        },
                        mentions: vec![TeamBoardMention {
                            identity: identity.clone(),
                            display: "@Reviewer".into(),
                            start: 0,
                            end: 9,
                        }],
                        notifications: vec![TeamBoardNotification {
                            identity: identity.clone(),
                            reasons: vec![TeamBoardNotificationReason::Mention],
                        }],
                    }],
                    ..Default::default()
                },
            },
        })
        .await?;
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (completed.thread_id, completed.turn.status),
        (thread.id.clone(), TurnStatus::Completed)
    );
    let requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
    let peer = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item.to_string().contains("please review this"))
        .expect("Board message reaches the model");
    assert_eq!(peer["role"], "assistant");
    let text = peer["content"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|part| part["text"].as_str())
        .expect("peer fragment has text");
    let payload: serde_json::Value = serde_json::from_str(
        text.lines()
            .find(|line| line.starts_with('{'))
            .expect("peer fragment JSON"),
    )?;
    assert_eq!(
        payload["board"],
        serde_json::json!({
            "conversationId": "board-message", "appendSeq": 5,
            "author": {"identity": {"type": "user"}, "name": "User", "machineName": null, "legacy": false},
            "mentions": [{"identity": {"type": "agent", "machineId": "remote", "serverId": "primary", "threadId": thread.id}, "display": "@Reviewer"}],
            "notificationReasons": ["mention"]
        })
    );

    // Turn completion and the host's durable receipt are separate events.
    // Wait for the committed receipt before asserting duplicate delivery.
    loop {
        let outbox: TeamOutboxListResponse = app
            .request(|request_id| ClientRequest::TeamOutboxList {
                request_id,
                params: TeamOutboxListParams {
                    coordinator_id: control.coordinator_id.clone(),
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        if outbox.data.iter().any(|entry| {
            entry.kind == "receipt"
                && entry.payload["deliveryId"] == delivery.id
                && entry.payload["state"] == "completed"
        }) {
            break;
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    }

    // The existing delivery identity and original body remain unchanged.
    let repeated: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: control.coordinator_id,
                delivery,
            },
        })
        .await?;
    assert_eq!(repeated.receipt.state, TeamReceiptState::Completed);
    Ok(())
}

#[tokio::test]
async fn watch_board_sync_stays_passive_during_a_later_ordinary_turn() -> Result<()> {
    let server =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "Ordinary task complete",
        )?])
        .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = TeamAddress {
        machine_id: "worker".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let identity = TeamBoardIdentity::Agent {
        address: address.clone(),
    };
    let control = TeamControlState {
        coordinator_id: "coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "Watcher".into(),
                control: control.clone(),
            },
        })
        .await?;
    let author = TeamBoardAuthor {
        identity: TeamBoardIdentity::User,
        name: "User".into(),
        machine_name: None,
        legacy: false,
    };
    let root = TeamMessage {
        id: "passive-root".into(),
        sender: TeamAddress {
            machine_id: "operator".into(),
            server_id: "operator".into(),
            thread_id: "operator".into(),
        },
        recipient: None,
        kind: TeamMessageKind::Board,
        body: "WATCH_ONLY_ROOT_BODY".into(),
        reply_to: None,
        topic: Some("review".into()),
        created_at: 10,
    };
    let reply = TeamMessage {
        id: "passive-reply".into(),
        reply_to: Some(root.id.clone()),
        body: "WATCH_ONLY_REPLY_BODY".into(),
        created_at: 11,
        ..root.clone()
    };
    let _: TeamSyncResponse = app
        .request(|request_id| ClientRequest::TeamSync {
            request_id,
            params: TeamSyncParams {
                administration: Vec::new(),
                retirements: Vec::new(),
                board_purges: Vec::new(),
                coordinator_id: control.coordinator_id.clone(),
                directory: Default::default(),
                message_status: Default::default(),
                board: vec![root.clone(), reply.clone()],
                cancellation_adapters: Vec::new(),
                board_state: TeamBoardSnapshot {
                    revision: 2,
                    entries: vec![
                        TeamBoardEntry {
                            message: root.clone(),
                            conversation_id: root.id.clone(),
                            append_seq: 1,
                            author: author.clone(),
                            mentions: Vec::new(),
                            notifications: Vec::new(),
                        },
                        TeamBoardEntry {
                            message: reply.clone(),
                            conversation_id: root.id.clone(),
                            append_seq: 2,
                            author: author.clone(),
                            mentions: Vec::new(),
                            notifications: Vec::new(),
                        },
                    ],
                    followers: vec![TeamBoardFollower {
                        mode: TeamBoardSubscriptionMode::Watch,
                        conversation_id: root.id.clone(),
                        identity: identity.clone(),
                        author: TeamBoardAuthor {
                            identity: identity.clone(),
                            name: "Watcher".into(),
                            machine_name: Some("Worker".into()),
                            legacy: false,
                        },
                    }],
                    topic_subscriptions: vec![TeamBoardTopicSubscription {
                        mode: TeamBoardSubscriptionMode::Watch,
                        topic: "review".into(),
                        identity,
                    }],
                    ..Default::default()
                },
            },
        })
        .await?;

    // Exercise the normal admission boundary after sync. A Watch implemented
    // as wake:false delivery would leak peer input into this explicit turn.
    let started: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Do the ordinary task".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (
            completed.thread_id,
            completed.turn.id,
            completed.turn.status
        ),
        (thread.id, started.turn.id, TurnStatus::Completed)
    );
    let requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
    let input = body["input"].to_string();
    assert!(input.contains("Do the ordinary task"));
    assert!(!input.contains("WATCH_ONLY_ROOT_BODY"));
    assert!(!input.contains("WATCH_ONLY_REPLY_BODY"));
    let outbox: TeamOutboxListResponse = app
        .request(|request_id| ClientRequest::TeamOutboxList {
            request_id,
            params: TeamOutboxListParams {
                coordinator_id: control.coordinator_id,
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert!(outbox.data.iter().all(|entry| entry.kind != "receipt"));
    Ok(())
}
