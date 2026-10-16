use std::sync::Arc;

use codex_core::StartIfIdleSubmission;
use codex_core::TurnInputRequest;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionMode;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrolled_agent_cannot_override_child_model_or_effort() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let recorded = responses::mount_sse_sequence(&server, vec![
        responses::sse(vec![responses::ev_response_created("spawn"),
            responses::ev_function_call_with_namespace("child-request", "multi_agent_v1", "spawn_agent",
                r#"{"message":"Inspect the patch","model":"gpt-5.1","reasoning_effort":"high"}"#),
            responses::ev_completed("spawn")]),
        responses::sse(vec![responses::ev_response_created("done"), responses::ev_completed("done")]),
    ]).await;
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::Collab)
                .unwrap();
            config
                .features
                .disable(codex_features::Feature::MultiAgentV2)
                .unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    gate.apply(test.session_configured.thread_id, 1, ExecutionMode::Running);
    test.codex
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Please spawn an agent to inspect the patch".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .function_call_output("child-request")
            .to_string()
            .contains("Team model and effort selections belong to the user")
    );
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_turn_does_not_consume_input_or_sample_and_resume_preserves_session()
-> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("after-resume"),
            responses::ev_completed("after-resume"),
        ]),
    )
    .await;
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    gate.apply(thread_id, 1, ExecutionMode::Paused);

    let input = || {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: "work after resume".to_string(),
            text_elements: Vec::new(),
        }])
    };
    let error = test
        .codex
        .start_turn_if_idle(input())
        .await
        .expect_err("held input must not start");
    assert!(error.to_string().contains("held by team control"));
    assert!(response.requests().is_empty());

    gate.apply(thread_id, 2, ExecutionMode::Running);
    test.codex.start_turn_if_idle(input()).await?;
    wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        response
            .single_request()
            .message_input_texts("user")
            .iter()
            .filter(|text| text.as_str() == "work after resume")
            .count(),
        1
    );
    Ok(())
}

#[test_case::test_case(ExecutionMode::Running; "resume")]
#[test_case::test_case(ExecutionMode::Stopped; "interrupt")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_finishes_the_current_request_and_holds_the_next(
    action: ExecutionMode,
) -> anyhow::Result<()> {
    let (release, response_gate) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("first")]),
            },
            StreamingSseChunk {
                gate: Some(response_gate),
                body: responses::sse(vec![responses::ev_completed("first")]),
            },
        ],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("second"),
                responses::ev_completed("second"),
            ]),
        }],
    ])
    .await;
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&server)
        .await?;
    let thread_id = test.session_configured.thread_id;
    gate.apply(thread_id, 1, ExecutionMode::Running);
    let input = |text: &str| {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }])
    };
    let StartIfIdleSubmission::Started { turn_id, .. } = test
        .codex
        .start_turn_if_idle(input("first request"))
        .await?
    else {
        anyhow::bail!("expected initial turn to start");
    };
    server.wait_for_request_count(/*count*/ 1).await;
    test.codex.steer_turn(input("follow up"), turn_id).await?;
    let mut changed = gate.subscribe();
    gate.apply(thread_id, 2, ExecutionMode::Paused);
    release.send(()).expect("first response is still in flight");

    // Observe completion through the execution counter, without timing sleeps.
    while gate.snapshot(thread_id).expect("enrolled root").in_flight != 0 {
        changed.changed().await?;
    }
    assert_eq!(server.requests().await.len(), 1);

    gate.apply(thread_id, 3, action);
    if action == ExecutionMode::Stopped {
        test.codex.submit(Op::Interrupt).await?;
        wait_for_event(test.codex.as_ref(), |event| {
            matches!(event, EventMsg::TurnAborted(_))
        })
        .await;
        assert_eq!(server.requests().await.len(), 1);
    } else {
        wait_for_event(test.codex.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(server.requests().await.len(), 2);
    }
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unavailable_team_model_rejects_new_work_without_consuming_input_or_falling_back()
-> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("chosen"),
            responses::ev_completed("chosen"),
        ]),
    )
    .await;
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_auto_env(&server)
        .await?;
    gate.apply(test.session_configured.thread_id, 1, ExecutionMode::Running);
    let original = test.codex.config_snapshot().await.model;
    let input = || {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Only run with my selected model".into(),
            text_elements: Vec::new(),
        }])
    };
    let mut unavailable = input();
    unavailable.thread_settings.model = Some("removed-team-model".into());
    let error = test
        .codex
        .start_turn_if_idle(unavailable)
        .await
        .expect_err("missing model must hold new work");
    assert!(
        error.to_string().contains("unavailable on this host"),
        "{error}"
    );
    assert!(response.requests().is_empty());
    assert_eq!(test.codex.config_snapshot().await.model, original);
    test.codex.start_turn_if_idle(input()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        response
            .single_request()
            .message_input_texts("user")
            .iter()
            .filter(|text| text.as_str() == "Only run with my selected model")
            .count(),
        1
    );
    Ok(())
}
