use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Result;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use super::*;

#[tokio::test]
async fn legacy_worker_is_rejected_before_control_sync_or_subscription_delivery() -> Result<()> {
    let home = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut methods = Vec::new();
        while let Some(message) = socket.next().await {
            let message = message?;
            if matches!(message, Message::Close(_)) {
                break;
            }
            let Message::Text(text) = message else {
                continue;
            };
            let request: Value = serde_json::from_str(&text)?;
            let Some(id) = request.get("id") else {
                continue;
            };
            let method = request["method"].as_str().unwrap();
            methods.push(method.to_string());
            let result = match method {
                "initialize" => {
                    json!({"userAgent":"fixture/1", "codexHome":"/fixture", "platformFamily":"unix", "platformOs":"linux"})
                }
                "team/hostRead" => {
                    json!({"installationId":"legacy", "protocolVersion":3, "control":null, "data":[], "runtime":[], "nextCursor":null})
                }
                method => anyhow::bail!("legacy worker received forbidden request {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id":id, "result":result}).to_string().into(),
                ))
                .await?;
        }
        Ok::<_, anyhow::Error>(methods)
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint.clone(),
            auth_token: None,
        },
        client_name: "protocol-version-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await?;
    let peer = Peer {
        handle: AppServerRequestHandle::Remote(client.request_handle()),
        verified: Arc::new(AtomicBool::new(false)),
    };
    let server = Server {
        id: "primary".into(),
        label: "Primary".into(),
        endpoint: crate::Endpoint::Websocket {
            url: endpoint,
            token_file: None,
        },
        cancellation_adapters: Vec::new(),
        environment_capabilities: Default::default(),
    };
    let machine = Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    };
    let coordinator = Coordinator::open(crate::tests::manifest(home.path())).await?;
    let error = coordinator
        .reconcile(&machine, &server, &peer)
        .await
        .expect_err("v3 cannot interpret Watch");
    client.shutdown().await?;
    assert_eq!(server_task.await??, vec!["initialize", "team/hostRead"]);
    assert!(
        error
            .to_string()
            .contains("worker team protocol 3 is incompatible with coordinator protocol 4")
    );
    assert!(!peer.verified.load(Ordering::Acquire));
    assert!(
        coordinator
            .store
            .get::<String>("installation", "worker/primary")
            .await?
            .is_none()
    );
    Ok(())
}
