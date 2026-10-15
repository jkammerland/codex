//! Legacy MCP stdio transport shared by separately authorized surfaces.

use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use std::future::Future;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

pub(crate) async fn serve<F, Fut>(tools: Value, call: F) -> Result<()>
where
    F: Fn(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    while let Some(line) = input.next_line().await? {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => dispatch(request, &tools, &call).await,
            Err(error) => Some(
                json!({"jsonrpc":"2.0", "id":null, "error":{"code":-32700, "message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            output.write_all(format!("{response}\n").as_bytes()).await?;
            output.flush().await?;
        }
    }
    Ok(())
}

async fn dispatch<F, Fut>(request: Value, tools: &Value, call: &F) -> Option<Value>
where
    F: Fn(Value) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let id = request.get("id")?.clone();
    let result = match request.get("method").and_then(Value::as_str) {
        // Advertise only the protocol this transport implements; clients may
        // negotiate down from a newer preferred version.
        Some("initialize") => Ok(
            json!({"protocolVersion":"2025-06-18", "capabilities":{"tools":{}},
            "serverInfo":{"name":"codex-team", "version":env!("CARGO_PKG_VERSION")}}),
        ),
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(json!({"tools":tools})),
        Some("tools/call") => Ok(match call(request).await {
            Ok(response) => {
                json!({"content":[{"type":"text", "text":response.to_string()}], "structuredContent":response, "isError":false})
            }
            Err(error) => {
                json!({"content":[{"type":"text", "text":error.to_string()}], "isError":true})
            }
        }),
        _ => Err(json!({"code":-32601,"message":"method not found"})),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
        Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":error}),
    })
}

#[cfg(test)]
#[path = "mcp_stdio_tests.rs"]
mod tests;
