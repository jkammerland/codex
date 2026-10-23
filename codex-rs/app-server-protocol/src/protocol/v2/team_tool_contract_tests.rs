use super::*;
use codex_code_mode_protocol::render_json_schema_to_typescript;
use pretty_assertions::assert_eq;

fn variant(schema: &Value, name: &str) -> Value {
    schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["properties"]["type"]["const"] == name)
        .unwrap()
        .clone()
}

#[test]
fn code_mode_declares_actual_payloads_for_team_actions() {
    let schema = team_agent_tool_schema();
    let complete = render_json_schema_to_typescript(&schema);
    assert!(complete.contains("recipient:"), "{complete}");
    assert!(complete.contains("acknowledge"), "{complete}");
    assert!(complete.contains("expectedRevision: number"), "{complete}");

    let send = render_json_schema_to_typescript(&variant(&schema, "send"));
    assert!(send.contains("recipient:"), "{send}");
    assert!(send.contains("body: string"), "{send}");
    assert!(send.contains("type: \"send\""), "{send}");
    assert!(send.contains("delivery?:"), "{send}");

    let board = &variant(&schema, "boardAction")["properties"]["request"];
    let read = render_json_schema_to_typescript(&variant(board, "readPage"));
    assert!(read.contains("conversationId: string"), "{read}");
    assert!(read.contains("mode?:"), "{read}");
    let ack = render_json_schema_to_typescript(&variant(board, "acknowledge"));
    assert!(ack.contains("token: string"), "{ack}");
    let publish = render_json_schema_to_typescript(&variant(&schema, "publish"));
    assert!(publish.contains("target:"), "{publish}");
    assert!(publish.contains("conversationId?:"), "{publish}");
    assert!(publish.contains("mentions?:"), "{publish}");
}

#[test]
fn canonical_calls_deserialize_with_defaults_and_legacy_calls_remain_accepted() -> anyhow::Result<()>
{
    let call: super::super::TeamAgentActionParams = serde_json::from_value(
        json!({"type":"send","recipient":"Reviewer","body":"Review the patch"}),
    )?;
    assert!(
        matches!(call, super::super::TeamAgentActionParams::Send {recipient:TeamAgentTarget::Alias(alias),kind:super::super::TeamMessageKind::Request,delivery:super::super::TeamDeliveryMode::NextTurn,..} if alias=="Reviewer")
    );
    let legacy: super::super::TeamAgentActionParams =
        serde_json::from_value(json!({"type":"post","topic":"builds","body":"Build finished"}))?;
    assert!(matches!(
        legacy,
        super::super::TeamAgentActionParams::Post { .. }
    ));
    let malformed = serde_json::from_value::<super::super::TeamAgentActionParams>(
        json!({"type":"send","body":"Missing recipient"}),
    );
    assert!(malformed.is_err());
    Ok(())
}

#[test]
fn complete_schema_retains_nested_required_fields() {
    let schema = team_tool_complete_schema(
        json!({"type":"object","properties":{"type":{"type":"string"},"query":{"type":"object","properties":{"action":{"type":"string"},"cursor":{"type":"string"}}}},"required":["type"],"oneOf":[{"properties":{"type":{"const":"explain"},"query":{"required":["action"]}},"required":["query"]}]}),
    );
    assert_eq!(schema["oneOf"][0]["required"], json!(["type", "query"]));
    let rendered = render_json_schema_to_typescript(&schema);
    assert!(rendered.contains("action: string"), "{rendered}");
    assert!(rendered.contains("cursor?: string"), "{rendered}");
}
