use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn create_wait_all_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "task_ids".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some("Accepted assignment IDs returned by spawn_agent or followup_task. Uses this fixed set; duplicate IDs are ignored.".to_string()),
            ),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::number(Some("Optional positive observation timeout in milliseconds. Omit for an indefinite event-driven wait. A timeout does not cancel work.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "wait_all".to_string(),
        description: "Wait quietly for every selected accepted assignment to reach a terminal state. Selected results accumulate without returning early. Normal waking direct messages, unselected results, and user input may return an incomplete snapshot; handle them and call again with the same task_ids. Queue-only progress does not interrupt. Held or unavailable work is reported without claiming failure. Returns task states and result references; completion means the accepted input reached a terminal turn, not that its requested outcome was fulfilled.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["task_ids".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "enum": ["all_done", "message", "user_input", "timeout", "held", "capacity_unavailable", "unavailable"] },
                "all_done": { "type": "boolean" },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task_id": { "type": "string" },
                            "task_name": { "type": ["string", "null"] },
                            "status": { "type": "string", "enum": ["queued", "running", "held", "capacity_unavailable", "completed", "failed", "cancelled", "unavailable"] },
                            "turn_id": { "type": ["string", "null"] },
                            "result_ref": { "type": ["string", "null"] }
                        },
                        "required": ["task_id", "task_name", "status", "turn_id", "result_ref"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["reason", "all_done", "tasks"],
            "additionalProperties": false
        }).into()),
    })
}
