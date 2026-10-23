//! Retiring a member blocks new turns without trapping its interrupted work.

use codex_core::RecoverTurnRequest;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInputRequest;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionMode;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::sync::oneshot;

#[tokio::test]
async fn retiring_member_can_resume_interrupted_work_only_after_operator_releases_hold()
-> anyhow::Result<()> {
    let (_release, first_response) = oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: Some(first_response),
            body: responses::sse_completed("interrupted"),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse_completed("recovered"),
        }],
    ])
    .await;
    let control = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(control.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    control.apply(thread_id, 1, ExecutionMode::Running);

    let user_input = || {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Finish this admitted task.".into(),
            text_elements: Vec::new(),
        }])
    };
    let StartIfIdleSubmission::Started { turn_id, .. } =
        test.codex.start_turn_if_idle(user_input()).await?
    else {
        panic!("initial work should start");
    };
    server.wait_for_request_count(/*count*/ 1).await;
    control.apply(thread_id, 2, ExecutionMode::Stopped);
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    control.begin_retirement(thread_id);

    let recovery = || RecoverTurnRequest {
        turn_id: turn_id.clone(),
        thread_settings: Default::default(),
        trace: None,
        cyber_access_program: None,
    };
    let held = test
        .codex
        .recover_turn_if_idle(recovery())
        .await
        .expect_err("operator hold must remain effective");
    assert!(matches!(
        held.details(),
        CodexErrorDetails::TeamExecutionHeld
    ));
    assert_eq!(server.requests().await.len(), 1);

    // Operator Resume permits the existing continuation, while new input stays fenced.
    control.apply(thread_id, 3, ExecutionMode::Running);
    let retired = test
        .codex
        .start_turn_if_idle(user_input())
        .await
        .expect_err("retirement must block new work");
    assert!(matches!(
        retired.details(),
        CodexErrorDetails::TeamExecutionHeld
    ));
    assert_eq!(
        test.codex.recover_turn_if_idle(recovery()).await?,
        StartIfIdleSubmission::Started {
            turn_id: turn_id.clone(),
            root_turn_id: turn_id.clone(),
        }
    );
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(event) if event.turn_id == turn_id),
    )
    .await;
    assert_eq!(server.requests().await.len(), 2);
    assert!(control.is_retiring(thread_id));
    server.shutdown().await;
    Ok(())
}
