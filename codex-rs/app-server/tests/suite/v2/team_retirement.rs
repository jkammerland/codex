//! Public retirement preserves operator holds and resumes only admitted work.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::*;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

async fn synchronize(
    app: &mut TestAppServer,
    retirement: &TeamMemberRetirement,
) -> Result<TeamHostReadResponse> {
    let _: TeamSyncResponse = app
        .request(|request_id| ClientRequest::TeamSync {
            request_id,
            params: TeamSyncParams {
                coordinator_id: "retirement-coordinator".into(),
                directory: Default::default(),
                board: Vec::new(),
                board_state: Default::default(),
                message_status: Default::default(),
                board_purges: Vec::new(),
                cancellation_adapters: Vec::new(),
                administration: Vec::new(),
                retirements: vec![retirement.clone()],
            },
        })
        .await?;
    app.request(|request_id| ClientRequest::TeamHostRead {
        request_id,
        params: TeamHostReadParams {
            cursor: None,
            limit: None,
        },
    })
    .await
}

async fn outbox(app: &mut TestAppServer) -> Result<TeamOutboxListResponse> {
    app.request(|request_id| ClientRequest::TeamOutboxList {
        request_id,
        params: TeamOutboxListParams {
            coordinator_id: "retirement-coordinator".into(),
            cursor: None,
            limit: None,
        },
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_retirement_waits_then_operator_resume_finishes_original_turn_before_removal()
-> Result<()> {
    let (_original_release, original_blocked) = oneshot::channel();
    let (recovery_release, recovery_blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("original")]),
            },
            StreamingSseChunk {
                gate: Some(original_blocked),
                body: create_final_assistant_message_sse_response("original blocked")?,
            },
        ],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("recovery")]),
            },
            StreamingSseChunk {
                gate: Some(recovery_blocked),
                body: create_final_assistant_message_sse_response("admitted work finished")?,
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
    let address = TeamAddress {
        machine_id: "local".into(),
        server_id: "primary".into(),
        thread_id: thread.id.clone(),
    };
    let control = TeamControlState {
        coordinator_id: "retirement-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "Finisher".into(),
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
                    text: "Finish this admitted task".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    model.wait_for_request_count(/*count*/ 1).await;
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

    let retirement = TeamMemberRetirement {
        operation_id: "retirement-one".into(),
        address: address.clone(),
        state: TeamMemberRetirementState::Retiring,
        waiting_reason: None,
    };
    let held = synchronize(&mut app, &retirement).await?;
    assert_eq!(
        held.control.as_ref().unwrap().mode,
        TeamExecutionMode::Stopped
    );
    assert_eq!(
        held.retirements[0].state,
        TeamMemberRetirementState::Retiring
    );
    assert_eq!(held.data.len(), 1);
    assert_eq!(model.requests().await.len(), 1);
    let pending: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: TeamDeliverParams {
                coordinator_id: control.coordinator_id.clone(),
                delivery: TeamDelivery {
                    id: "unadmitted-mail".into(),
                    thread_id: thread.id.clone(),
                    mode: TeamDeliveryMode::NextTurn,
                    wake: true,
                    message: TeamMessage {
                        id: "late-message".into(),
                        sender: address.clone(),
                        recipient: Some(address),
                        kind: TeamMessageKind::Request,
                        body: "Do not start another task".into(),
                        reply_to: None,
                        topic: None,
                        created_at: 1,
                    },
                },
            },
        })
        .await?;
    assert_eq!(pending.receipt.state, TeamReceiptState::Rejected);

    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: TeamControlState {
                    revision: 3,
                    mode: TeamExecutionMode::Running,
                    ..control
                },
            },
        })
        .await?;
    model.wait_for_request_count(/*count*/ 2).await;
    let draining = synchronize(&mut app, &retirement).await?;
    assert_eq!(
        draining.retirements[0].state,
        TeamMemberRetirementState::Retiring
    );
    recovery_release.send(()).unwrap();
    let completed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(
        (completed.turn.id, completed.turn.status),
        (started.turn.id, TurnStatus::Completed)
    );
    let removed = loop {
        let host = synchronize(&mut app, &retirement).await?;
        if host.retirements[0].state == TeamMemberRetirementState::Removed {
            break host;
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    };
    assert!(removed.data.is_empty());
    assert_eq!(removed.control.unwrap().mode, TeamExecutionMode::Running);
    assert_eq!(model.requests().await.len(), 2);
    model.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_abandons_failed_delivery_without_replaying_it_and_original_chat_can_continue()
-> Result<()> {
    const PEER_REQUEST: &str = "Review the change whose result the operator may abandon";
    const REJECTED_INPUT: &str = "This new task must wait for removal";
    let model = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &model,
        vec![
            responses::sse_failed("failed-review", "invalid_prompt", "Review request failed"),
            create_final_assistant_message_sse_response("The original chat is available again")?,
        ],
    )
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&model.uri()).write(home.path())?;
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
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                operation_id: None,
                address: address.clone(),
                alias: "Reviewer".into(),
                control: TeamControlState {
                    coordinator_id: "retirement-coordinator".into(),
                    revision: 1,
                    mode: TeamExecutionMode::Running,
                },
            },
        })
        .await?;
    let delivery = TeamDeliverParams {
        coordinator_id: "retirement-coordinator".into(),
        delivery: TeamDelivery {
            id: "failed-delivery".into(),
            thread_id: thread.id.clone(),
            mode: TeamDeliveryMode::NextTurn,
            wake: true,
            message: TeamMessage {
                id: "failed-message".into(),
                sender: TeamAddress {
                    thread_id: "peer".into(),
                    ..address.clone()
                },
                recipient: Some(address.clone()),
                kind: TeamMessageKind::Request,
                body: PEER_REQUEST.into(),
                reply_to: None,
                topic: None,
                created_at: 1,
            },
        },
    };
    let _: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: delivery.clone(),
        })
        .await?;
    let failed: TurnCompletedNotification = app.read_notification("turn/completed").await?;
    assert_eq!(failed.thread_id, thread.id);
    assert_eq!(failed.turn.status, TurnStatus::Failed);

    // Public delivery evidence must establish the unresolved failure before retirement.
    let failed_receipt = loop {
        let receipts = outbox(&mut app)
            .await?
            .data
            .into_iter()
            .filter(|entry| entry.kind == "receipt")
            .map(|entry| serde_json::from_value::<TeamReceipt>(entry.payload))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(receipt) = receipts.into_iter().find(|receipt| {
            receipt.delivery_id == delivery.delivery.id
                && receipt.state == TeamReceiptState::NeedsReconciliation
                && receipt
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.contains("Review request failed"))
        }) {
            break receipt;
        }
        let _: TeamChangedNotification = app.read_notification("team/changed").await?;
    };
    assert_eq!(failed_receipt.turn_id, Some(failed.turn.id.clone()));
    let retirement = TeamMemberRetirement {
        operation_id: "retire-failed-reviewer".into(),
        address,
        state: TeamMemberRetirementState::Retiring,
        waiting_reason: None,
    };
    let held = synchronize(&mut app, &retirement).await?;
    assert_eq!(held.data.len(), 1);
    assert_eq!(
        held.retirements,
        vec![TeamMemberRetirement {
            waiting_reason: Some("Waiting for admitted delivery evidence to settle.".into()),
            ..retirement.clone()
        }]
    );
    let unresolved: TeamDeliverResponse = app
        .request(|request_id| ClientRequest::TeamDeliver {
            request_id,
            params: delivery.clone(),
        })
        .await?;
    assert_eq!(
        unresolved.receipt.state,
        TeamReceiptState::NeedsReconciliation
    );
    assert_eq!(unresolved.receipt.turn_id, failed_receipt.turn_id);
    let denied = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: REJECTED_INPUT.into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let denied = app
        .read_stream_until_error_message(RequestId::Integer(denied))
        .await?;
    assert_eq!(denied.error.code, -32600);
    assert_eq!(
        denied.error.data,
        Some(serde_json::json!({ "reason": "teamExecutionHeld" }))
    );
    assert_eq!(requests.requests().len(), 1);

    // This explicit operator decision removes the retirement blocker without changing the receipt.
    let params = TeamDeliveryAbandonParams {
        operation_id: "c77513cc-6241-4c67-a58a-334323c71c09".into(),
        coordinator_id: "retirement-coordinator".into(),
        thread_id: thread.id.clone(),
        delivery_id: delivery.delivery.id.clone(),
        reason: "The operator accepts the incomplete review and wants to continue the chat".into(),
    };
    let abandoned: TeamDeliveryAbandonResponse = app
        .request(|request_id| ClientRequest::TeamDeliveryAbandon {
            request_id,
            params: params.clone(),
        })
        .await?;
    assert_eq!(
        abandoned.abandonment,
        TeamDeliveryAbandonment {
            operation_id: params.operation_id.clone(),
            delivery_id: params.delivery_id.clone(),
            thread_id: params.thread_id.clone(),
            reason: params.reason.clone(),
        }
    );
    let removed = synchronize(&mut app, &retirement).await?;
    assert!(removed.data.is_empty());
    assert_eq!(
        removed.retirements,
        vec![TeamMemberRetirement {
            state: TeamMemberRetirementState::Removed,
            ..retirement
        }]
    );
    let replay: TeamDeliveryAbandonResponse = app
        .request(|request_id| ClientRequest::TeamDeliveryAbandon { request_id, params })
        .await?;
    assert_eq!(replay, abandoned);
    let evidence = outbox(&mut app).await?;
    let abandonments = evidence
        .data
        .iter()
        .filter(|entry| entry.kind == "delivery_abandonment")
        .map(|entry| serde_json::from_value::<TeamDeliveryAbandonment>(entry.payload.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(abandonments, vec![abandoned.abandonment]);
    let final_receipt = evidence
        .data
        .iter()
        .filter(|entry| entry.kind == "receipt")
        .map(|entry| serde_json::from_value::<TeamReceipt>(entry.payload.clone()))
        .collect::<Result<Vec<_>, _>>()?
        .pop()
        .expect("the original failure receipt remains published");
    assert_eq!(final_receipt, unresolved.receipt);
    assert_eq!(requests.requests().len(), 1);

    let continued = app
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Continue this original chat after removing the teammate".into(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    assert_eq!(continued.thread_id, thread.id);
    assert_eq!(continued.turn.status, TurnStatus::Completed);
    assert_ne!(continued.turn.id, failed.turn.id);
    let after_continuation = outbox(&mut app).await?;
    assert_eq!(
        after_continuation
            .data
            .iter()
            .filter(|entry| entry.kind == "receipt")
            .collect::<Vec<_>>(),
        evidence
            .data
            .iter()
            .filter(|entry| entry.kind == "receipt")
            .collect::<Vec<_>>()
    );

    // History retains the failed input once; neither recovery nor its retry resubmits it.
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body_json()["input"]
                .to_string()
                .matches(PEER_REQUEST)
                .count())
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.body_contains_text(REJECTED_INPUT))
    );
    Ok(())
}
