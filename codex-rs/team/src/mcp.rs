use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use serde_json::json;

#[derive(Debug, Parser)]
pub struct McpCommand {
    #[arg(long, required_unless_present = "url", conflicts_with = "url")]
    socket: Option<PathBuf>,
    #[arg(long, required_unless_present = "socket", conflicts_with = "socket")]
    url: Option<String>,
    /// Environment variable containing the worker's transport credential.
    #[arg(long)]
    auth_token_env: Option<String>,
    #[arg(long)]
    thread: String,
    /// Environment variable containing this agent's bound team credential.
    #[arg(long, default_value = "CODEX_TEAM_AGENT_CREDENTIAL")]
    credential_env: String,
}

pub(crate) async fn run(command: McpCommand) -> Result<()> {
    let credential = std::env::var(&command.credential_env)
        .context("agent credential environment variable is missing")?;
    let endpoint = match command.socket {
        Some(socket) => RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::try_from(socket)?,
        },
        None => {
            let url = command.url.context("worker URL is required")?;
            crate::config::validate_url(&url, &["ws", "wss"])?;
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: url,
                auth_token: command
                    .auth_token_env
                    .map(|name| {
                        std::env::var(name)
                            .context("worker credential environment variable is missing")
                    })
                    .transpose()?,
            }
        }
    };
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint,
        client_name: "codex-team-agent-mcp".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 64,
    })
    .await?;
    let tools = json!([{
        "name":"team", "description":TEAM_TOOL_DESCRIPTION,
        "inputSchema":team_agent_tool_schema(), "outputSchema":team_agent_output_schema()
    }]);
    let bridge_id = uuid::Uuid::new_v4().to_string();
    let bridge_id = bridge_id.as_str();
    let (client_ref, thread, credential_ref) =
        (&client, command.thread.as_str(), credential.as_str());
    crate::mcp_stdio::serve(tools, |request| async move {
        Ok(
            call(client_ref, thread, credential_ref, bridge_id, &request)
                .await?
                .result,
        )
    })
    .await?;
    client.shutdown().await?;
    Ok(())
}

async fn call(
    client: &RemoteAppServerClient,
    thread_id: &str,
    credential: &str,
    bridge_id: &str,
    request: &Value,
) -> Result<TeamAgentRequestResponse> {
    anyhow::ensure!(
        request.pointer("/params/name").and_then(Value::as_str) == Some("team"),
        "unknown tool"
    );
    let action: TeamAgentActionParams = serde_json::from_value(
        request
            .pointer("/params/arguments")
            .context("tool arguments are missing")?
            .clone(),
    )?;
    Ok(client
        .request_typed(ClientRequest::TeamAgentRequest {
            request_id: crate::coordinator::request_id(),
            params: TeamAgentRequestParams {
                thread_id: thread_id.into(),
                operation_id: Some(
                    uuid::Uuid::new_v5(
                        &uuid::Uuid::NAMESPACE_OID,
                        format!(
                            "{bridge_id}:{}",
                            request.get("id").context("tool request id is missing")?
                        )
                        .as_bytes(),
                    )
                    .to_string(),
                ),
                credential: credential.into(),
                action,
            },
        })
        .await?)
}
