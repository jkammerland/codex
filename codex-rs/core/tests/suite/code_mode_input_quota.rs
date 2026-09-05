#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use serde_json::Value;

const MAX_INPUT_ITEMS: usize = 64;
const MAX_INPUT_ITEM_TOKENS: usize = 900;
const MAX_INPUT_TOTAL_TOKENS: usize = 4_000;
const MAX_INPUT_TEXT_BYTES: usize = 4 * 1024;
const MAX_INPUT_TOTAL_BYTES: usize = 16 * 1024;

async fn run_notification_turn(
    server: &wiremock::MockServer,
    test: &TestCodex,
    response_index: usize,
) -> Result<ResponsesRequest> {
    let call_id = format!("quota-call-{response_index}");
    let response_id = format!("quota-response-{response_index}");
    let completion_id = format!("quota-completion-{response_index}");
    let code = r#"
for (let index = 0; index < 80; index++) {
    await notify(`quota-notification-${index}:` + "x".repeat(300));
}
text("done");
"#;
    responses::mount_sse_once(
        server,
        sse(vec![
            ev_response_created(&response_id),
            ev_custom_tool_call(&call_id, "exec", code),
            ev_completed(&response_id),
        ]),
    )
    .await;
    let followup = responses::mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message(&completion_id, "done"),
            ev_completed(&completion_id),
        ]),
    )
    .await;

    test.submit_turn("emit sequential code-mode notifications")
        .await?;
    Ok(followup.single_request())
}

fn quota_notification_items(request: &ResponsesRequest, response_index: usize) -> Vec<Value> {
    let expected_call_id = format!("quota-call-{response_index}");
    request
        .inputs_of_type("custom_tool_call_output")
        .into_iter()
        .filter(|item| item.get("name").and_then(serde_json::Value::as_str) == Some("exec"))
        .filter(|item| {
            item.get("call_id").and_then(serde_json::Value::as_str)
                == Some(expected_call_id.as_str())
        })
        .filter(|item| {
            item.get("output")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|output| output.starts_with("quota-notification-"))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_notifications_share_a_bounded_quota_and_reset_for_the_next_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        let _ = config.features.enable(Feature::CodeMode);
    });
    let test = builder.build_with_auto_env(&server).await?;

    for response_index in 1..=2 {
        let request = run_notification_turn(&server, &test, response_index).await?;
        let notification_items = quota_notification_items(&request, response_index);
        let notifications = notification_items
            .iter()
            .filter_map(|item| item.get("output").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            !notifications.is_empty(),
            "a fresh model turn should receive admitted notifications"
        );
        assert!(notifications.len() <= MAX_INPUT_ITEMS);
        assert!(
            notifications
                .iter()
                .all(|text| text.len() <= MAX_INPUT_TEXT_BYTES)
        );
        assert!(
            notifications.iter().map(|text| text.len()).sum::<usize>() <= MAX_INPUT_TOTAL_BYTES,
            "sequential active notification awaits must share one aggregate quota"
        );
        let item_tokens = notification_items
            .iter()
            .map(|item| {
                serde_json::to_vec(item)
                    .expect("notification input should serialize")
                    .len()
                    .div_ceil(4)
            })
            .collect::<Vec<_>>();
        assert!(
            item_tokens
                .iter()
                .all(|tokens| *tokens <= MAX_INPUT_ITEM_TOKENS),
            "the final stamped request items must respect the per-item token cap"
        );
        assert!(
            item_tokens.iter().sum::<usize>() <= MAX_INPUT_TOTAL_TOKENS,
            "the final stamped request items must respect the shared sample token cap"
        );
        assert!(notification_items.iter().all(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
                && item
                    .pointer("/internal_chat_message_metadata_passthrough/turn_id")
                    .and_then(Value::as_str)
                    .is_some_and(|turn_id| !turn_id.is_empty())
                && item
                    .pointer("/internal_chat_message_metadata_passthrough/create_time")
                    .is_some()
        }));
    }

    Ok(())
}
