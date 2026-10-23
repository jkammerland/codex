use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::*;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

#[tokio::test]
async fn paused_host_queues_one_delivery_and_resumes_the_original_thread() -> Result<()> {
    let server =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "reviewed",
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
    let control = TeamControlState {
        coordinator_id: "test-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Paused,
    };
    let address = TeamAddress {
        machine_id: "local".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let enrolled: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "reviewer".into(),
                control: control.clone(),
            },
        })
        .await?;
    assert_eq!(enrolled.enrollment.address, address);
    let params = TeamDeliverParams {
        coordinator_id: control.coordinator_id.clone(),
        delivery: TeamDelivery {
            id: "delivery-one".into(),
            thread_id: thread.id.clone(),
            mode: TeamDeliveryMode::NextTurn,
            wake: true,
            message: TeamMessage {
                id: "message-one".into(),
                sender: TeamAddress {
                    thread_id: "sender".into(),
                    ..address.clone()
                },
                recipient: Some(address),
                kind: TeamMessageKind::Request,
                body: "Please review the change".into(),
                reply_to: None,
                topic: None,
                created_at: 100,
            },
        },
    };
    let first: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: params.clone(),
        })
        .await?;
    let replay: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(replay, first);
    assert_eq!(first.receipt.state, TeamReceiptState::Pending);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/responses"))
    );

    let resumed = TeamControlState {
        revision: 2,
        mode: TeamExecutionMode::Running,
        ..control
    };
    let response: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: resumed.clone(),
            },
        })
        .await?;
    assert_eq!(
        response,
        TeamControlResponse {
            control: resumed,
            reconciled: true
        }
    );
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
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
        .find(|item| item.to_string().contains("Please review the change"))
        .expect("peer input reaches model");
    assert_eq!(peer["role"], "assistant");

    // Enrollment supplies authorization once through trusted context, while
    // incoming mail keeps its attributed peer role.
    let authorization = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item.to_string().contains("<team_authorization>"))
        .collect::<Vec<_>>();
    assert_eq!(authorization.len(), 1);
    assert_eq!(authorization[0]["role"], "developer");
    assert!(
        authorization[0]
            .to_string()
            .contains("Board cleanup through team tools are authorized")
    );
    assert!(
        authorization[0]
            .to_string()
            .contains("Models, execution controls, and approvals remain user-owned")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_resumes_the_original_turn_once_before_later_mail() -> Result<()> {
    let (_release, blocked) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("first")]),
            },
            StreamingSseChunk {
                gate: Some(blocked),
                body: responses::sse(vec![responses::ev_completed("first")]),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: create_final_assistant_message_sse_response("continued")?,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: create_final_assistant_message_sse_response("mail handled")?,
        }],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = TeamAddress {
        machine_id: "local".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let control = TeamControlState {
        coordinator_id: "test-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "builder".into(),
                control: control.clone(),
            },
        })
        .await?;
    let started: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Work on the original task".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    server.wait_for_request_count(/*count*/ 1).await;
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: TeamControlState {
                    revision: 2,
                    mode: TeamExecutionMode::Stopped,
                    ..control.clone()
                },
            },
        })
        .await?;
    let interrupted: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (interrupted.turn.id, interrupted.turn.status),
        (started.turn.id.clone(), TurnStatus::Interrupted)
    );
    let _: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: control.coordinator_id.clone(),
                delivery: TeamDelivery {
                    id: "later-delivery".into(),
                    thread_id: thread.id.clone(),
                    mode: TeamDeliveryMode::NextTurn,
                    wake: true,
                    message: TeamMessage {
                        id: "later-message".into(),
                        sender: TeamAddress {
                            thread_id: "peer".into(),
                            ..address.clone()
                        },
                        recipient: Some(address.clone()),
                        kind: TeamMessageKind::Request,
                        body: "Later request".into(),
                        reply_to: None,
                        topic: None,
                        created_at: 101,
                    },
                },
            },
        })
        .await?;
    let resumed = TeamControlState {
        revision: 3,
        mode: TeamExecutionMode::Running,
        ..control
    };
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: resumed.clone(),
            },
        })
        .await?;
    let continuation: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (continuation.turn.id, continuation.turn.status),
        (started.turn.id.clone(), TurnStatus::Completed)
    );
    let later: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_ne!(later.turn.id, started.turn.id);
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams { control: resumed },
        })
        .await?;
    let host: TeamHostReadResponse = app
        .request(|request_id| ClientRequest::TeamHostRead {
            request_id,
            params: TeamHostReadParams {
                cursor: None,
                limit: None,
            },
        })
        .await?;
    assert_eq!(host.data[0].continuation_turn_id, None);
    assert_eq!(server.requests().await.len(), 3);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn agent_credential_cannot_impersonate_another_enrolled_session() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let first = app.start_thread(ThreadStartParams::default()).await?.thread;
    let second = app.start_thread(ThreadStartParams::default()).await?.thread;
    let control = TeamControlState {
        coordinator_id: "test-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    for (thread, alias) in [(&first, "builder"), (&second, "reviewer")] {
        let _: TeamEnrollResponse = app
            .request(|request_id| ClientRequest::TeamEnroll {
                request_id,
                params: TeamEnrollParams {
                    operation_id: None,
                    address: TeamAddress {
                        machine_id: "local".into(),
                        server_id: "primary".into(),
                        thread_id: thread.id.clone(),
                    },
                    alias: alias.into(),
                    control: control.clone(),
                },
            })
            .await?;
    }
    let credential: TeamAgentCredentialResponse = app
        .request(|request_id| ClientRequest::TeamAgentCredential {
            request_id,
            params: TeamAgentCredentialParams {
                thread_id: first.id.clone(),
            },
        })
        .await?;
    let request_id = app
        .send_request(
            "team/agentRequest",
            Some(serde_json::to_value(TeamAgentRequestParams {
                operation_id: None,
                thread_id: second.id.clone(),
                credential: credential.credential.clone(),
                action: TeamAgentActionParams::Directory {
                    query: None,
                    details: true,
                    cursor: None,
                    limit: None,
                },
            })?),
        )
        .await?;
    let forged = app
        .read_stream_until_error_message(RequestId::Integer(request_id))
        .await?;
    assert_eq!(forged.error.message, "invalid agent credential");
    let valid: TeamAgentRequestResponse = app
        .request(|request_id| ClientRequest::TeamAgentRequest {
            request_id,
            params: TeamAgentRequestParams {
                operation_id: None,
                thread_id: first.id,
                credential: credential.credential,
                action: TeamAgentActionParams::Directory {
                    query: None,
                    details: true,
                    cursor: None,
                    limit: None,
                },
            },
        })
        .await?;
    assert_eq!(valid.result["data"], serde_json::json!([]));
    Ok(())
}
