use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::*;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use super::*;

#[tokio::test]
async fn pending_stop_cleanup_keeps_the_same_connection_for_ordered_resume() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server_task = tokio::spawn(async move {
        // Accept exactly one transport. Both revisions must be acknowledged on
        // it even though cleanup remains deliberately unsettled throughout.
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut admitted = Vec::new();
        while let Some(message) = socket.next().await {
            let message = message?;
            if matches!(message, Message::Close(_)) {
                break;
            }
            let Message::Text(text) = message else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text)?;
            let result = match request["method"].as_str() {
                Some("initialize") => json!({"userAgent":"fixture/1"}),
                Some("team/control") => {
                    let params: TeamControlParams =
                        serde_json::from_value(request["params"].clone())?;
                    admitted.push(params.control.clone());
                    serde_json::to_value(TeamControlResponse {
                        control: params.control,
                        reconciled: false,
                    })?
                }
                _ => continue,
            };
            socket
                .send(Message::Text(
                    json!({"id":request["id"], "result":result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, anyhow::Error>(admitted)
    });
    let server = Server {
        environment_capabilities: Default::default(),
        id: "primary".into(),
        label: "Primary".into(),
        endpoint: crate::Endpoint::Websocket {
            url: endpoint.clone(),
            token_file: None,
        },
        cancellation_adapters: Vec::new(),
    };
    let machine = Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    };
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![machine.clone()];
    let coordinator = Coordinator::open(manifest).await?;
    let original = coordinator.current_control().await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        })
        .await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        })
        .await?;
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint,
            auth_token: None,
        },
        client_name: "team-control-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await?;
    let peer = Peer {
        handle: AppServerRequestHandle::Remote(client.request_handle()),
        verified: Arc::new(AtomicBool::new(true)),
    };

    coordinator
        .controls(&machine, &server, &peer, Some(original), false)
        .await?;
    assert_eq!(
        coordinator.statuses.read().await["worker/primary"].applied_revision,
        Some(2)
    );
    client.shutdown().await?;
    let admitted = server_task.await??;
    assert_eq!(
        admitted
            .iter()
            .map(|control| (control.revision, control.mode))
            .collect::<Vec<_>>(),
        vec![
            (1, TeamExecutionMode::Stopped),
            (2, TeamExecutionMode::Running)
        ]
    );
    Ok(())
}
