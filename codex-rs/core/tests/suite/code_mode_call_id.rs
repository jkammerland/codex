#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

async fn assert_invalid_code_mode_call_id_fails_without_a_correlated_context_item(
    invalid_call_id: String,
    expected_error: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        let _ = config.features.enable(Feature::CodeMode);
    });
    let test = builder.build_with_auto_env(&server).await?;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("invalid-call-id-response"),
            ev_custom_tool_call(&invalid_call_id, "exec", "text('unreachable');"),
            ev_completed("invalid-call-id-response"),
        ]),
    )
    .await;
    let recovery_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("invalid-call-id-followup", "unexpected"),
            ev_completed("invalid-call-id-followup"),
        ]),
    )
    .await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "reject an invalid code-mode call ID".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut output_token_counts = Vec::new();
    let mut rejected_raw_items = Vec::new();
    let mut fatal_error = None;
    wait_for_event(&test.codex, |event| match event {
        EventMsg::RawResponseItem(raw)
            if matches!(
                &raw.item,
                ResponseItem::CustomToolCall { call_id, .. } if call_id == &invalid_call_id
            ) =>
        {
            rejected_raw_items.push(raw.item.clone());
            false
        }
        EventMsg::RawResponseItem(raw)
            if matches!(
                &raw.item,
                ResponseItem::CustomToolCallOutput { .. } | ResponseItem::FunctionCallOutput { .. }
            ) =>
        {
            output_token_counts.push(approx_token_count(
                &serde_json::to_string(&raw.item).expect("serialize response item"),
            ));
            false
        }
        EventMsg::Error(error) => {
            fatal_error = Some(error.message.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert!(
        rejected_raw_items
            .iter()
            .all(|item| item.id().is_some_and(|id| !id.is_empty())),
        "the audited raw call should retain its assigned response item ID",
    );
    for item in &mut rejected_raw_items {
        item.set_id(None);
    }
    assert_eq!(
        rejected_raw_items,
        vec![ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: invalid_call_id.clone(),
            name: "exec".to_string(),
            namespace: None,
            input: "text('unreachable');".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }],
        "the rejected raw call must remain observable exactly once",
    );
    assert!(
        output_token_counts.iter().all(|tokens| *tokens <= 10_000),
        "no invalid correlated output may enter model context: {output_token_counts:?}"
    );
    assert!(
        fatal_error
            .as_deref()
            .is_some_and(|error| error.contains(expected_error)),
        "invalid correlation ID should fail the turn out of band: {fatal_error:?}"
    );
    assert!(
        recovery_response.requests().is_empty(),
        "an invalid correlation ID must not produce another model request"
    );
    test.submit_turn("continue after the rejected tool call")
        .await?;
    let recovery_request = recovery_response.single_request();
    assert!(
        recovery_request
            .inputs_of_type("custom_tool_call")
            .into_iter()
            .all(
                |item| item.get("call_id").and_then(serde_json::Value::as_str)
                    != Some(invalid_call_id.as_str())
            ),
        "the rejected incoming tool call must not be committed to model history"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_code_mode_call_id_fails_without_a_correlated_context_item() -> Result<()> {
    assert_invalid_code_mode_call_id_fails_without_a_correlated_context_item(
        String::new(),
        "code-mode tool call ID must not be empty",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_code_mode_call_id_fails_without_a_correlated_context_item() -> Result<()> {
    let oversized_call_id = "c".repeat(40 * 1024 + 1);
    let expected_error = format!(
        "code-mode tool call ID is {} bytes; maximum is {} bytes",
        oversized_call_id.len(),
        codex_code_mode::grpc::MAX_IDENTIFIER_BYTES,
    );
    assert_invalid_code_mode_call_id_fails_without_a_correlated_context_item(
        oversized_call_id,
        &expected_error,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_and_maximum_code_mode_call_ids_remain_correlated() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        let _ = config.features.enable(Feature::CodeMode);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let call_ids = [
        "ordinary-call-id".to_string(),
        "m".repeat(codex_code_mode::grpc::MAX_IDENTIFIER_BYTES),
    ];

    for (index, call_id) in call_ids.into_iter().enumerate() {
        let response_id = format!("legal-call-id-response-{index}");
        let completion_id = format!("legal-call-id-completion-{index}");
        responses::mount_sse_once(
            &server,
            sse(vec![
                ev_response_created(&response_id),
                ev_custom_tool_call(&call_id, "exec", "text('correlated');"),
                ev_completed(&response_id),
            ]),
        )
        .await;
        let followup = responses::mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message(&completion_id, "done"),
                ev_completed(&completion_id),
            ]),
        )
        .await;

        test.submit_turn("run code mode with a bounded call ID")
            .await?;
        let output = followup.single_request().custom_tool_call_output(&call_id);
        assert_eq!(output["call_id"].as_str(), Some(call_id.as_str()));
    }

    Ok(())
}
