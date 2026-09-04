use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::DefaultDaemonSocket;
use super::probe_default_daemon_socket;
use crate::version::CODEX_CLI_VERSION;

async fn serve_daemon_probe(
    mut listener: codex_uds::UnixListener,
    codex_home: PathBuf,
) -> Result<()> {
    let probe_stream = listener.accept().await?;
    drop(probe_stream);
    let stream = listener.accept().await?;
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
            "userAgent": format!("codex_app_server/{CODEX_CLI_VERSION} (test)"),
            "codexHome": codex_home,
            "platformFamily": "windows",
            "platformOs": "windows",
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

fn is_elevated_test_process() -> Result<bool> {
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
        ])
        .output()?;
    anyhow::ensure!(output.status.success(), "elevation query failed");
    match String::from_utf8(output.stdout)?.trim() {
        "True" => Ok(true),
        "False" => Ok(false),
        other => anyhow::bail!("unexpected elevation result: {other}"),
    }
}

fn broaden_directory_acl(directory: &std::path::Path) -> Result<()> {
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            r#"
$ErrorActionPreference = 'Stop'
$acl = Get-Acl -LiteralPath $env:CODEX_TEST_DIRECTORY
$everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new($everyone, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
$acl.AddAccessRule($rule)
Set-Acl -LiteralPath $env:CODEX_TEST_DIRECTORY -AclObject $acl
"#,
        ])
        .env("CODEX_TEST_DIRECTORY", directory)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "failed to broaden test ACL: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn discovery_rejects_unprotected_socket_before_connecting() -> Result<()> {
    let codex_home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())?;
    let socket_parent = socket_path.as_path().parent().context("socket parent")?;
    codex_uds::prepare_private_socket_directory(socket_parent).await?;
    broaden_directory_acl(socket_parent)?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;

    let result = probe_default_daemon_socket(codex_home.path()).await;
    assert!(matches!(result, DefaultDaemonSocket::Unavailable { .. }));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "untrusted listener unexpectedly received a connection"
    );
    Ok(())
}

#[tokio::test]
async fn discovery_authenticates_a_private_same_user_peer() -> Result<()> {
    let codex_home = TempDir::new()?;
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home.path())?;
    codex_uds::prepare_private_socket_directory(
        socket_path.as_path().parent().context("socket parent")?,
    )
    .await?;
    let listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let server = tokio::spawn(serve_daemon_probe(
        listener,
        codex_home.path().to_path_buf(),
    ));

    let result = probe_default_daemon_socket(codex_home.path()).await;
    if is_elevated_test_process()? {
        assert!(matches!(result, DefaultDaemonSocket::Unavailable { .. }));
        server.abort();
    } else {
        assert_eq!(result, DefaultDaemonSocket::Compatible(socket_path));
        server.await??;
    }
    Ok(())
}
