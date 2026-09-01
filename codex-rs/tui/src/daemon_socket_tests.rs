use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::maybe_probe_default_daemon_socket;
use crate::version::CODEX_CLI_VERSION;

async fn serve_daemon_probe(
    listener: UnixListener,
    codex_home: PathBuf,
    version: String,
) -> Result<()> {
    let (stream, _) = listener.accept().await?;
    let mut websocket = accept_async(stream).await?;
    let Message::Text(payload) = websocket
        .next()
        .await
        .transpose()?
        .context("missing initialize request")?
    else {
        anyhow::bail!("expected text initialize request");
    };
    let request: codex_app_server_protocol::JSONRPCRequest = serde_json::from_str(&payload)?;
    let response = serde_json::json!({
        "id": request.id,
        "result": {
            "userAgent": format!("codex_app_server/{version} (test)"),
            "codexHome": codex_home,
            "platformFamily": "unix",
            "platformOs": "linux",
        },
    });
    websocket
        .send(Message::Text(response.to_string().into()))
        .await?;
    websocket
        .next()
        .await
        .transpose()?
        .context("missing initialized notification")?;
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_skips_missing_socket() -> Result<()> {
    let codex_home = TempDir::new()?;
    assert_eq!(
        maybe_probe_default_daemon_socket(codex_home.path()).await,
        None
    );
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_accepts_matching_runtime() -> Result<()> {
    let codex_home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())
        .context("control socket path")?;
    std::fs::create_dir_all(
        socket_path
            .as_path()
            .parent()
            .context("missing socket parent")?,
    )?;
    let listener = UnixListener::bind(socket_path.as_path())?;
    let server = tokio::spawn(serve_daemon_probe(
        listener,
        codex_home.path().to_path_buf(),
        CODEX_CLI_VERSION.to_string(),
    ));

    assert_eq!(
        maybe_probe_default_daemon_socket(codex_home.path()).await,
        Some(socket_path)
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_skips_incompatible_runtime() -> Result<()> {
    let codex_home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())
        .context("control socket path")?;
    std::fs::create_dir_all(
        socket_path
            .as_path()
            .parent()
            .context("missing socket parent")?,
    )?;
    let listener = UnixListener::bind(socket_path.as_path())?;
    let server = tokio::spawn(serve_daemon_probe(
        listener,
        codex_home.path().to_path_buf(),
        "0.151.0".to_string(),
    ));

    assert_eq!(
        maybe_probe_default_daemon_socket(codex_home.path()).await,
        None
    );
    server.await??;
    Ok(())
}
