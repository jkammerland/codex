use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::DefaultDaemonSocket;
use super::probe_default_daemon_socket;
use crate::version::CODEX_CLI_VERSION;

async fn serve_daemon_probe(
    listener: UnixListener,
    codex_home: PathBuf,
    version: String,
    response_delay: Duration,
) -> Result<()> {
    let (probe_stream, _) = listener.accept().await?;
    drop(probe_stream);
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
    tokio::time::sleep(response_delay).await;
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
        probe_default_daemon_socket(codex_home.path()).await,
        DefaultDaemonSocket::Absent
    );
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_skips_stale_socket() -> Result<()> {
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
    drop(listener);

    assert_eq!(
        probe_default_daemon_socket(codex_home.path()).await,
        DefaultDaemonSocket::Absent
    );
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_rejects_regular_file() -> Result<()> {
    let codex_home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())
        .context("control socket path")?;
    std::fs::create_dir_all(
        socket_path
            .as_path()
            .parent()
            .context("missing socket parent")?,
    )?;
    std::fs::write(socket_path.as_path(), "not a socket")?;

    assert_eq!(
        probe_default_daemon_socket(codex_home.path()).await,
        DefaultDaemonSocket::Unavailable {
            socket_path,
            reason: "the control path is not a stale socket".to_string(),
        }
    );
    Ok(())
}

async fn assert_matching_daemon_is_accepted(response_delay: Duration) -> Result<()> {
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
        response_delay,
    ));

    assert_eq!(
        probe_default_daemon_socket(codex_home.path()).await,
        DefaultDaemonSocket::Compatible(socket_path)
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn default_daemon_auto_connect_accepts_matching_runtime() -> Result<()> {
    assert_matching_daemon_is_accepted(Duration::ZERO).await
}

#[tokio::test]
async fn default_daemon_handshake_can_exceed_previous_outer_timeout() -> Result<()> {
    assert_matching_daemon_is_accepted(Duration::from_millis(300)).await
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
        Duration::ZERO,
    ));

    assert_eq!(
        probe_default_daemon_socket(codex_home.path()).await,
        DefaultDaemonSocket::Incompatible {
            socket_path,
            reported_version: Some("0.151.0".to_string()),
        }
    );
    server.await??;
    Ok(())
}
