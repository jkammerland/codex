//! Public API checks for passive mail, independent holds and turn settings.
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::*;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio::sync::oneshot;

async fn enroll(app: &mut TestAppServer, thread_id: &str) -> Result<TeamAddress> {
    let address = TeamAddress {
        machine_id: "local".into(),
        server_id: "primary".into(),
        thread_id: thread_id.into(),
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "reviewer".into(),
                control: control(1, TeamExecutionMode::Running),
            },
        })
        .await?;
    Ok(address)
}

fn control(revision: i64, mode: TeamExecutionMode) -> TeamControlState {
    TeamControlState {
        coordinator_id: "coordinator".into(),
        revision,
        mode,
    }
}

async fn host(app: &mut TestAppServer) -> Result<TeamHostReadResponse> {
    app.request(|request_id| ClientRequest::TeamHostRead {
        request_id,
        params: TeamHostReadParams {
            cursor: None,
            limit: None,
        },
    })
    .await
}

async fn deliver(app: &mut TestAppServer, address: TeamAddress, wake: bool) -> Result<TeamReceipt> {
    let response: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: "coordinator".into(),
                delivery: TeamDelivery {
                    id: "delivery".into(),
                    thread_id: address.thread_id.clone(),
                    mode: TeamDeliveryMode::NextTurn,
                    wake,
                    message: TeamMessage {
                        id: "message".into(),
                        sender: TeamAddress {
                            thread_id: "sender".into(),
                            ..address.clone()
                        },
                        recipient: Some(address),
                        kind: if wake {
                            TeamMessageKind::Request
                        } else {
                            TeamMessageKind::Note
                        },
                        body: "Peer context for the next turn".into(),
                        reply_to: None,
                        topic: None,
                        created_at: 1,
                    },
                },
            },
        })
        .await?;
    Ok(response.receipt)
}

async fn user_turn(app: &mut TestAppServer, thread_id: &str) -> Result<TurnStartResponse> {
    app.request(|request_id| ClientRequest::TurnStart {
        request_id,
        params: TurnStartParams {
            thread_id: thread_id.into(),
            input: vec![UserInput::Text {
                text: "Continue with my request".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        },
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passive_safe_boundary_note_waits_for_the_next_ordinary_turn() -> Result<()> {
    const NOTE: &str = "passive status update from an older client";
    let (release, blocked) = oneshot::channel();
    let (follow_up_release, follow_up_blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("working")]),
            },
            StreamingSseChunk {
                gate: Some(blocked),
                body: create_final_assistant_message_sse_response("original work finished")?,
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("follow-up")]),
            },
            StreamingSseChunk {
                gate: Some(follow_up_blocked),
                body: create_final_assistant_message_sse_response("ordinary work finished")?,
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: create_final_assistant_message_sse_response("passive note handled")?,
        }],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = enroll(&mut app, &thread.id).await?;
    let first = user_turn(&mut app, &thread.id).await?.turn;
    model.wait_for_request_count(/*count*/ 1).await;

    // Already persisted older envelopes still have to honor passive delivery.
    let response: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: "coordinator".into(),
                delivery: TeamDelivery {
                    id: "passive-delivery".into(),
                    thread_id: thread.id.clone(),
                    mode: TeamDeliveryMode::SafeBoundary,
                    wake: false,
                    message: TeamMessage {
                        id: "passive-message".into(),
                        sender: TeamAddress {
                            thread_id: "sender".into(),
                            ..address.clone()
                        },
                        recipient: Some(address),
                        kind: TeamMessageKind::Note,
                        body: NOTE.into(),
                        reply_to: None,
                        topic: None,
                        created_at: 1,
                    },
                },
            },
        })
        .await?;
    assert_eq!(response.receipt.state, TeamReceiptState::Pending);
    release.send(()).unwrap();
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.turn.id, first.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(model.requests().await.len(), 1);

    // Ordinary input is sampled first. Pending team input is then consumed at
    // the next boundary within that same user turn, without starting a new turn.
    let second = user_turn(&mut app, &thread.id).await?.turn;
    model.wait_for_request_count(/*count*/ 2).await;
    assert_eq!(accepted(&mut app).await?.turn_id, Some(second.id.clone()));
    follow_up_release.send(()).unwrap();
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.turn.id, second.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let requests = model
        .requests()
        .await
        .iter()
        .map(|body| serde_json::from_slice::<Value>(body))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        requests
            .iter()
            .map(|request| request["input"].to_string().matches(NOTE).count())
            .collect::<Vec<_>>(),
        vec![0, 0, 1]
    );
    model.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_discovery_reports_local_executor_before_any_turn() -> Result<()> {
    let model = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        // The usual test environment is a remote executor and cannot cover native discovery.
        .without_auto_env()
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app
        .request(|request_id| ClientRequest::ThreadStart {
            request_id,
            params: ThreadStartParams::default(),
        })
        .await?;
    enroll(&mut app, &thread.id).await?;

    let snapshot = host(&mut app).await?;
    let member = snapshot
        .runtime
        .iter()
        .find(|member| member.thread_id == thread.id)
        .expect("enrolled local runtime");
    assert_eq!(member.activity, TeamActivity::Idle);
    assert_eq!(member.turn_id, None);
    assert_eq!(member.environments.len(), 1);
    let environment = &member.environments[0];
    assert_eq!(environment.id, "local");
    assert_eq!(environment.os.as_deref(), Some(std::env::consts::OS));
    assert!(
        environment
            .shell
            .as_ref()
            .is_some_and(|shell| !shell.is_empty())
    );
    assert!(!environment.current);
    assert!(environment.next);
    assert!(
        model
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_team_rejects_start_and_steer_without_consuming_input() -> Result<()> {
    let (model, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: None,
        body: create_final_assistant_message_sse_response("explicitly resumed")?,
    }]])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    enroll(&mut app, &thread.id).await?;

    // Both entry points must reject input before it reaches history or inference.
    for (revision, mode) in [
        (2, TeamExecutionMode::Paused),
        (3, TeamExecutionMode::Stopped),
    ] {
        let _: TeamControlResponse = app
            .request(|request_id| ClientRequest::TeamControl {
                request_id,
                params: TeamControlParams {
                    control: control(revision, mode),
                },
            })
            .await?;
        for method in ["turn/start", "turn/steer"] {
            let mut params = serde_json::json!({
                "threadId": thread.id,
                "clientUserMessageId": format!("rejected-{revision}-{method}"),
                "input": [{ "type": "text", "text": "rejected held input", "textElements": [] }],
            });
            if method == "turn/steer" {
                params["expectedTurnId"] = "unstarted-turn".into();
            }
            let request_id = app.send_request(method, Some(params)).await?;
            let error = app
                .read_stream_until_error_message(RequestId::Integer(request_id))
                .await?;
            assert_eq!(
                error,
                JSONRPCError {
                    id: RequestId::Integer(request_id),
                    error: JSONRPCErrorError {
                        code: -32600,
                        message: "session execution is held by team control".into(),
                        data: Some(serde_json::json!({ "reason": "teamExecutionHeld" })),
                    },
                }
            );
        }
        assert!(model.requests().await.is_empty());
        assert_eq!(host(&mut app).await?.runtime[0].turn_id, None);
    }

    // Resume alone cannot replay rejected input; explicitly submit a fresh turn.
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: control(4, TeamExecutionMode::Running),
            },
        })
        .await?;
    let started = user_turn(&mut app, &thread.id).await?;
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(completed.turn.id, started.turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let requests = model.requests().await;
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0])?;
    assert!(!body["input"].to_string().contains("rejected held input"));
    assert!(
        body["input"]
            .to_string()
            .contains("Continue with my request")
    );
    model.shutdown().await;
    Ok(())
}

async fn accepted(app: &mut TestAppServer) -> Result<TeamReceipt> {
    loop {
        let outbox: TeamOutboxListResponse = app
            .request(|request_id| ClientRequest::TeamOutboxList {
                request_id,
                params: TeamOutboxListParams {
                    coordinator_id: "coordinator".into(),
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        for entry in outbox.data {
            if entry.kind == "receipt" {
                let receipt: TeamReceipt = serde_json::from_value(entry.payload)?;
                if receipt.state == TeamReceiptState::Accepted {
                    return Ok(receipt);
                }
            }
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passive_note_waits_for_a_natural_turn_and_settings_leave_active_model_unchanged()
-> Result<()> {
    let (release, blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("first")]),
            },
            StreamingSseChunk {
                gate: Some(blocked),
                body: create_final_assistant_message_sse_response("handled")?,
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: create_final_assistant_message_sse_response("handled note")?,
        }],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = enroll(&mut app, &thread.id).await?;
    assert_eq!(
        deliver(&mut app, address, false).await?.state,
        TeamReceiptState::Pending
    );
    assert_eq!(host(&mut app).await?.runtime[0].turn_id, None);
    assert!(model.requests().await.is_empty());

    let started = user_turn(&mut app, &thread.id).await?;
    model.wait_for_request_count(1).await;
    let receipt = accepted(&mut app).await?;
    assert_eq!(receipt.turn_id, Some(started.turn.id.clone()));
    let _: ThreadSettingsUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                model: Some("next-model".into()),
                effort: Some(codex_protocol::openai_models::ReasoningEffort::High),
                ..Default::default()
            },
        })
        .await?;
    let member = host(&mut app).await?.runtime.remove(0);
    assert_eq!(member.current_model.as_deref(), Some("mock-model"));
    assert_eq!(member.next_model.as_deref(), Some("next-model"));
    assert_eq!(member.next_effort.as_deref(), Some("high"));
    release.send(()).unwrap();
    let finished: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(finished.turn.id, started.turn.id);
    let requests = model.requests().await;
    let body: Value = serde_json::from_slice(requests.last().unwrap())?;
    assert_eq!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item.to_string().contains("Peer context for the next turn"))
            .count(),
        1
    );
    model.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_resume_preserves_a_manual_interrupt_until_explicit_user_input() -> Result<()> {
    let (_release, blocked) = oneshot::channel();
    let (resume_release, resumed_blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("held")]),
            },
            StreamingSseChunk {
                gate: Some(blocked),
                body: responses::sse(vec![responses::ev_completed("held")]),
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("resumed")]),
            },
            StreamingSseChunk {
                gate: Some(resumed_blocked),
                body: create_final_assistant_message_sse_response("resumed")?,
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: create_final_assistant_message_sse_response("mail handled")?,
        }],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = enroll(&mut app, &thread.id).await?;
    let started = user_turn(&mut app, &thread.id).await?;
    model.wait_for_request_count(1).await;
    let _: TurnInterruptResponse = app
        .request(|request_id| ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: thread.id.clone(),
                turn_id: started.turn.id,
            },
        })
        .await?;
    let aborted: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(aborted.turn.status, TurnStatus::Interrupted);
    for (revision, mode) in [
        (2, TeamExecutionMode::Paused),
        (3, TeamExecutionMode::Running),
    ] {
        let _: TeamControlResponse = app
            .request(|request_id| ClientRequest::TeamControl {
                request_id,
                params: TeamControlParams {
                    control: control(revision, mode),
                },
            })
            .await?;
    }
    assert_eq!(
        deliver(&mut app, address, true).await?.state,
        TeamReceiptState::Pending
    );
    let member = host(&mut app).await?.runtime.remove(0);
    assert!(member.manually_held);
    assert_eq!(member.turn_id, None);
    assert_eq!(model.requests().await.len(), 1);

    let resumed = user_turn(&mut app, &thread.id).await?;
    assert_eq!(
        accepted(&mut app).await?.turn_id,
        Some(resumed.turn.id.clone())
    );
    assert!(!host(&mut app).await?.runtime[0].manually_held);
    resume_release.send(()).unwrap();
    let finished: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(finished.turn.id, resumed.turn.id);
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unavailable_model_keeps_mail_pending_until_an_explicit_supported_selection() -> Result<()>
{
    let model = app_test_support::create_mock_responses_server_sequence(vec![
        create_final_assistant_message_sse_response("handled")?,
    ])
    .await;
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"removed-team-model\"\napproval_policy = \"never\"\nopenai_base_url = \"{}/v1\"\ncli_auth_credentials_store = \"file\"\n",
            model.uri()
        ),
    )?;
    codex_login::login_with_api_key(
        home.path(),
        "test-api-key",
        codex_login::AuthCredentialsStoreMode::File,
        codex_login::AuthKeyringBackendKind::default(),
    )?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = enroll(&mut app, &thread.id).await?;
    deliver(&mut app, address, true).await?;
    loop {
        let outbox: TeamOutboxListResponse = app
            .request(|request_id| ClientRequest::TeamOutboxList {
                request_id,
                params: TeamOutboxListParams {
                    coordinator_id: "coordinator".into(),
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        if outbox.data.iter().any(|entry| {
            entry.kind == "receipt"
                && entry.payload["detail"]
                    .as_str()
                    .is_some_and(|text| text.contains("unavailable on this host"))
        }) {
            break;
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    }
    let member = host(&mut app).await?.runtime.remove(0);
    assert_eq!(member.turn_id, None);
    assert!(
        member
            .model_error
            .unwrap()
            .contains("unavailable on this host")
    );
    assert!(
        model
            .received_requests()
            .await
            .unwrap()
            .iter()
            // OpenAI startup may prewarm a WebSocket with a GET upgrade.
            // No inference POST may run while this selection is unavailable.
            .all(|request| request.method.as_str() != "POST"
                || !request.url.path().ends_with("/responses"))
    );
    let catalog: ModelListResponse = app
        .request(|request_id| ClientRequest::ModelList {
            request_id,
            params: ModelListParams {
                cursor: None,
                limit: None,
                include_hidden: None,
            },
        })
        .await?;
    let selected = catalog
        .data
        .first()
        .expect("test host has its bundled catalog");
    let _: ThreadSettingsUpdateResponse = app
        .request(|request_id| ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: thread.id.clone(),
                model: Some(selected.model.clone()),
                effort: Some(selected.default_reasoning_effort.clone()),
                ..Default::default()
            },
        })
        .await?;
    let finished: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(finished.turn.status, TurnStatus::Completed);
    assert_eq!(
        model
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "POST"
                && request.url.path().ends_with("/responses"))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_safe_boundary_input_is_not_completed_by_an_unsampled_same_id_continuation()
-> Result<()> {
    const PEER: &str = "unique peer input awaiting a safe boundary";
    let (_first_release, first_blocked) = oneshot::channel();
    let (resume_release, resume_blocked) = oneshot::channel();
    let (peer_release, peer_blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("original")]),
            },
            StreamingSseChunk {
                gate: Some(first_blocked),
                body: create_final_assistant_message_sse_response("original blocked")?,
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("continued")]),
            },
            StreamingSseChunk {
                gate: Some(resume_blocked),
                body: create_final_assistant_message_sse_response("original continued")?,
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("peer")]),
            },
            StreamingSseChunk {
                gate: Some(peer_blocked),
                body: create_final_assistant_message_sse_response("peer handled")?,
            },
        ],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let address = enroll(&mut app, &thread.id).await?;
    let original = user_turn(&mut app, &thread.id).await?.turn;
    model.wait_for_request_count(/*count*/ 1).await;
    let _: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: "coordinator".into(),
                delivery: TeamDelivery {
                    id: "safe-delivery".into(),
                    thread_id: thread.id.clone(),
                    mode: TeamDeliveryMode::SafeBoundary,
                    wake: true,
                    message: TeamMessage {
                        id: "safe-message".into(),
                        sender: address.clone(),
                        recipient: Some(address),
                        kind: TeamMessageKind::Request,
                        body: PEER.into(),
                        reply_to: None,
                        topic: None,
                        created_at: 1,
                    },
                },
            },
        })
        .await?;
    let queued = accepted(&mut app).await?;
    assert_eq!(
        (queued.delivery_id.as_str(), queued.turn_id.as_deref()),
        ("safe-delivery", Some(original.id.as_str()))
    );
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: control(2, TeamExecutionMode::Stopped),
            },
        })
        .await?;
    let interrupted: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (interrupted.turn.id, interrupted.turn.status),
        (original.id.clone(), TurnStatus::Interrupted)
    );
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: control(3, TeamExecutionMode::Running),
            },
        })
        .await?;
    model.wait_for_request_count(/*count*/ 2).await;
    resume_release.send(()).unwrap();
    let continued: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (continued.turn.id, continued.turn.status),
        (original.id.clone(), TurnStatus::Completed)
    );
    model.wait_for_request_count(/*count*/ 3).await;

    // The continuation reused the turn ID without the peer input. Its finish
    // must not complete that delivery; the restored copy now owns a new turn.
    let outbox: TeamOutboxListResponse = app
        .request(|request_id| ClientRequest::TeamOutboxList {
            request_id,
            params: TeamOutboxListParams {
                coordinator_id: "coordinator".into(),
                cursor: None,
                limit: None,
            },
        })
        .await?;
    let receipts = outbox
        .data
        .into_iter()
        .filter(|entry| entry.kind == "receipt")
        .map(|entry| serde_json::from_value::<TeamReceipt>(entry.payload))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.state != TeamReceiptState::Completed)
    );
    let requests = model
        .requests()
        .await
        .into_iter()
        .map(|body| serde_json::from_slice::<Value>(&body))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        requests
            .iter()
            .map(|body| body.to_string().matches(PEER).count())
            .collect::<Vec<_>>(),
        vec![0, 0, 1]
    );
    peer_release.send(()).unwrap();
    let handled: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_ne!(handled.turn.id, original.id);
    assert_eq!(handled.turn.status, TurnStatus::Completed);
    loop {
        let outbox: TeamOutboxListResponse = app
            .request(|request_id| ClientRequest::TeamOutboxList {
                request_id,
                params: TeamOutboxListParams {
                    coordinator_id: "coordinator".into(),
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        let receipts = outbox
            .data
            .into_iter()
            .filter(|entry| entry.kind == "receipt")
            .map(|entry| serde_json::from_value::<TeamReceipt>(entry.payload))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(completed) = receipts
            .into_iter()
            .find(|receipt| receipt.state == TeamReceiptState::Completed)
        {
            assert_eq!(completed.turn_id, Some(handled.turn.id));
            break;
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    }
    model.shutdown().await;
    Ok(())
}
