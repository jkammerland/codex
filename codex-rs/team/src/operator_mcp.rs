use crate::TeamClient;
use crate::TeamRequest;
use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::TeamExecutionMode;
use serde_json::Value;
use serde_json::json;

pub(crate) async fn run(client: TeamClient) -> Result<()> {
    let tools = json!([{"name":"team_control", "description":"Operator-only team status, Pause, Resume and Stop. Stop also requests cancellation of owned work; offline hosts remain unconfirmed.",
        "inputSchema":{"type":"object", "properties":{"action":{"type":"string", "enum":["status","pause","resume","stop"]}}, "required":["action"], "additionalProperties":false}}]);
    let client = &client;
    crate::mcp_stdio::serve(tools, |request| async move {
        anyhow::ensure!(
            request.pointer("/params/name").and_then(Value::as_str) == Some("team_control"),
            "unknown tool"
        );
        let request = match request
            .pointer("/params/arguments/action")
            .and_then(Value::as_str)
            .context("action is missing")?
        {
            "status" => TeamRequest::Snapshot,
            "pause" => TeamRequest::Control {
                mode: TeamExecutionMode::Paused,
            },
            "resume" => TeamRequest::Control {
                mode: TeamExecutionMode::Running,
            },
            "stop" => TeamRequest::Control {
                mode: TeamExecutionMode::Stopped,
            },
            _ => anyhow::bail!("unknown operator action"),
        };
        Ok(serde_json::to_value(client.request(request).await?)?)
    })
    .await
}
