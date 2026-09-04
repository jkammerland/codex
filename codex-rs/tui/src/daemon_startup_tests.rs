//! Implicit daemon discovery is opportunistic; explicit endpoints remain authoritative.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

async fn serve_app_server_probe(
    listener: tokio::net::TcpListener,
    codex_home: PathBuf,
    version: &str,
) -> color_eyre::Result<()> {
    let (stream, _) = listener.accept().await?;
    let mut websocket = accept_async(stream).await?;
    let Message::Text(payload) = websocket
        .next()
        .await
        .transpose()?
        .ok_or_else(|| color_eyre::eyre::eyre!("missing initialize request"))?
    else {
        color_eyre::eyre::bail!("expected text initialize request");
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
        .ok_or_else(|| color_eyre::eyre::eyre!("missing initialized notification"))?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn daemon_connection_rejects_unprotected_socket_before_handshake() -> color_eyre::Result<()> {
    let home = TempDir::new()?;
    let parent = home.path().join("control");
    std::fs::create_dir(&parent)?;
    let socket_path = AbsolutePathBuf::from_absolute_path_checked(parent.join("server.sock"))?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
    };
    tokio::select! {
        result = app_server_connection::connect(&target) => assert!(result.is_err()),
        _ = listener.accept() => panic!("unprotected listener must not receive a connection"),
    }
    Ok(())
}

#[tokio::test]
async fn daemon_startup_falls_back_only_for_implicit_endpoints() -> color_eyre::Result<()> {
    for scenario in ["missing socket", "failed handshake", "explicit endpoint"] {
        let home = TempDir::new()?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = if scenario == "missing socket" {
            RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path_checked(
                    home.path().join("gone.sock"),
                )?,
            }
        } else {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: format!("ws://{}", listener.local_addr()?),
                auth_token: None,
            }
        };
        let reject_handshake = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let mut target = if scenario == "explicit endpoint" {
            AppServerTarget::Remote { endpoint }
        } else {
            AppServerTarget::LocalDaemon { endpoint }
        };
        let original_target = target.clone();
        let mut state_db = None;
        let result = start_app_server(
            &mut target,
            Arg0DispatchPaths::default(),
            config,
            Vec::new(),
            LoaderOverrides::default(),
            /*strict_config*/ false,
            CloudConfigBundleLoader::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            &mut state_db,
            Arc::new(EnvironmentManager::default_for_tests()),
        )
        .await;
        reject_handshake.abort();
        if scenario == "explicit endpoint" {
            assert!(result.is_err());
            assert_eq!(target, original_target);
            assert!(state_db.is_none());
        } else {
            let server = AppServerSession::new(result?, target.thread_params_mode());
            assert!(server.uses_embedded_app_server());
            assert_eq!(target, AppServerTarget::Embedded);
            assert!(state_db.is_some());
            server.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn daemon_startup_preserves_incompatible_connection_error() -> color_eyre::Result<()> {
    let home = TempDir::new()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = RemoteAppServerEndpoint::WebSocket {
        websocket_url: format!("ws://{}", listener.local_addr()?),
        auth_token: None,
    };
    let server = tokio::spawn(serve_app_server_probe(
        listener,
        home.path().to_path_buf(),
        "incompatible-test-version",
    ));
    let mut target = AppServerTarget::LocalDaemon { endpoint };
    let original_target = target.clone();
    let mut state_db = None;

    let result = start_app_server(
        &mut target,
        Arg0DispatchPaths::default(),
        config,
        Vec::new(),
        LoaderOverrides::default(),
        /*strict_config*/ false,
        CloudConfigBundleLoader::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        &mut state_db,
        Arc::new(EnvironmentManager::default_for_tests()),
    )
    .await;
    let error = match result {
        Ok(app_server) => {
            let _ = app_server.shutdown().await;
            color_eyre::eyre::bail!("incompatible daemon unexpectedly fell back to embedded");
        }
        Err(error) => error,
    };

    assert!(
        error
            .downcast_ref::<app_server_connection::IncompatibleLocalDaemon>()
            .is_some()
    );
    assert_eq!(target, original_target);
    assert!(state_db.is_none());
    server.await??;
    Ok(())
}
