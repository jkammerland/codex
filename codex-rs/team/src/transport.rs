use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use axum::extract::ws::Message as BrowserMessage;
use axum::extract::ws::WebSocket;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_uds::UnixStream;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::SinkExt;
use futures::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::config::Endpoint;

#[derive(Clone)]
pub(crate) struct ResolvedEndpoint {
    pub endpoint: RemoteAppServerEndpoint,
    // The coordinator and any interactive relays retain the forwarding process.
    _tunnel: Option<Arc<Child>>,
}

fn ssh_forward_command(destination: &str, local_port: u16, remote_port: u16) -> Command {
    let mut command = Command::new("ssh");
    command
        .args([
            "-v",
            "-N",
            "-o",
            "BatchMode=yes",
            // An SSH alias must not erase the forwarding this transport owns.
            "-o",
            "ClearAllForwardings=no",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectTimeout=10",
            "-L",
        ])
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
        .arg("--")
        .arg(destination);
    command
}

impl ResolvedEndpoint {
    pub async fn open(endpoint: &Endpoint) -> Result<Self> {
        let (endpoint, tunnel) = match endpoint {
            Endpoint::Local { socket } => (
                RemoteAppServerEndpoint::UnixSocket {
                    socket_path: AbsolutePathBuf::try_from(socket.clone())?,
                },
                None,
            ),
            Endpoint::Websocket { url, token_file } => (
                RemoteAppServerEndpoint::WebSocket {
                    websocket_url: url.clone(),
                    auth_token: token(token_file.as_deref()).await?,
                },
                None,
            ),
            Endpoint::Ssh {
                destination,
                remote_port,
                token_file,
            } => {
                let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let local_port = reservation.local_addr()?.port();
                drop(reservation);
                let mut child = ssh_forward_command(destination, local_port, *remote_port)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .context("could not start SSH forwarding")?;
                let stderr = child
                    .stderr
                    .take()
                    .context("SSH diagnostic stream is missing")?;
                let (ready, readiness) = oneshot::channel();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    let mut ready = Some(ready);
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.contains("Local forwarding listening on")
                            && let Some(ready) = ready.take()
                        {
                            let _ = ready.send(());
                        }
                    }
                });
                readiness.await.context("SSH forwarding failed; check the configured host, key and running worker listener")?;
                (
                    RemoteAppServerEndpoint::WebSocket {
                        websocket_url: format!("ws://127.0.0.1:{local_port}"),
                        auth_token: token(token_file.as_deref()).await?,
                    },
                    Some(Arc::new(child)),
                )
            }
        };
        Ok(Self {
            endpoint,
            _tunnel: tunnel,
        })
    }

    /// Each attached TUI gets its own worker connection and RPC identity space.
    pub async fn relay(&self, browser: WebSocket) -> Result<()> {
        codex_utils_rustls_provider::ensure_rustls_crypto_provider();
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(128 << 20))
            .max_frame_size(Some(128 << 20));
        match &self.endpoint {
            RemoteAppServerEndpoint::UnixSocket { socket_path } => {
                let stream = UnixStream::connect(socket_path.as_path()).await?;
                let (remote, _) = tokio_tungstenite::client_async_with_config(
                    "ws://localhost/rpc",
                    stream,
                    Some(config),
                )
                .await?;
                forward(browser, remote).await
            }
            RemoteAppServerEndpoint::WebSocket {
                websocket_url,
                auth_token,
            } => {
                let mut request = websocket_url.as_str().into_client_request()?;
                if let Some(token) = auth_token {
                    request
                        .headers_mut()
                        .insert("authorization", format!("Bearer {token}").parse()?);
                }
                let connector =
                    codex_http_client::maybe_build_rustls_client_config_with_custom_ca()?
                        .map(tokio_tungstenite::Connector::Rustls);
                let (remote, _) = tokio_tungstenite::connect_async_tls_with_config(
                    request,
                    Some(config),
                    false,
                    connector,
                )
                .await?;
                forward(browser, remote).await
            }
        }
    }
}

pub(crate) async fn token(path: Option<&std::path::Path>) -> Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let token = tokio::fs::read_to_string(path).await?.trim().to_string();
    ensure!(!token.is_empty(), "credential file is empty");
    Ok(Some(token))
}

async fn forward<S: AsyncRead + AsyncWrite + Unpin>(
    mut browser: WebSocket,
    mut remote: WebSocketStream<S>,
) -> Result<()> {
    loop {
        tokio::select! {
            inbound = browser.recv() => match inbound {
                Some(Ok(BrowserMessage::Text(text))) => remote.send(Message::Text(text.as_str().into())).await?,
                Some(Ok(BrowserMessage::Binary(bytes))) => remote.send(Message::Binary(bytes)).await?,
                Some(Ok(BrowserMessage::Ping(bytes))) => browser.send(BrowserMessage::Pong(bytes)).await?,
                Some(Ok(BrowserMessage::Pong(_))) => {},
                Some(Ok(BrowserMessage::Close(_))) | None => break,
                Some(Err(error)) => return Err(error.into()),
            },
            outbound = remote.next() => match outbound {
                Some(Ok(Message::Text(text))) => browser.send(BrowserMessage::Text(text.as_str().into())).await?,
                Some(Ok(Message::Binary(bytes))) => browser.send(BrowserMessage::Binary(bytes)).await?,
                Some(Ok(Message::Ping(bytes))) => remote.send(Message::Pong(bytes)).await?,
                Some(Ok(Message::Pong(_) | Message::Frame(_))) => {},
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(error)) => return Err(error.into()),
            },
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
