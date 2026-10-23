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
