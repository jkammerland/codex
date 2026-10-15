use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[tokio::test]
async fn transport_negotiates_supported_protocol_and_preserves_call_errors_and_ids() {
    let tools = json!([{"name":"team"}]);
    let call =
        |_request: Value| async { Err::<Value, _>(anyhow::anyhow!("invalid agent credential")) };
    let initialized = super::dispatch(
        json!({"id":1, "method":"initialize", "params":{"protocolVersion":"unknown-future"}}),
        &tools,
        &call,
    )
    .await
    .unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    let response = super::dispatch(
        json!({"id":"original-id", "method":"tools/call"}),
        &tools,
        &call,
    )
    .await
    .unwrap();
    assert_eq!(
        response,
        json!({"jsonrpc":"2.0","id":"original-id", "result":{"content":[{"type":"text","text":"invalid agent credential"}],"isError":true}})
    );
    assert!(
        super::dispatch(json!({"method":"notifications/initialized"}), &tools, &call)
            .await
            .is_none()
    );
}
