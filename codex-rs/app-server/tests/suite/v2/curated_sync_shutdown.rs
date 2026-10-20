//! Graceful shutdown owns Git transports started by the production plugin warmup.

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::time::timeout;

#[tokio::test]
async fn shutdown_reaps_blocked_curated_git_before_returning() -> Result<()> {
    let fixture = TempDir::new()?;
    let codex_home = TempDir::new()?;
    let transport = TcpListener::bind("127.0.0.1:0").await?;
    let transport_url = format!("http://{}/plugins.git", transport.local_addr()?);
    let git_config = fixture.path().join("gitconfig");
    std::fs::write(
        &git_config,
        format!(
            "[url \"{transport_url}\"]\n\tinsteadOf = https://github.com/openai/plugins.git\n[http]\n\tproxy =\n"
        ),
    )?;
    let empty_git_config = fixture.path().join("empty-gitconfig");
    std::fs::write(&empty_git_config, "")?;
    let responses = core_test_support::responses::start_mock_server().await;
    MockResponsesConfig::new(&responses.uri())
        .enable_feature(Feature::Plugins)
        .disable_feature(Feature::RemotePlugin)
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_plugin_startup_tasks()
        .with_env_overrides(&[
            (
                "GIT_CONFIG_GLOBAL",
                Some(git_config.to_str().context("Git config path")?),
            ),
            (
                "GIT_CONFIG_SYSTEM",
                Some(empty_git_config.to_str().context("empty Git config path")?),
            ),
        ])
        .build_initialized_with_timeout(Duration::from_secs(10))
        .await?;

    // Receiving the complete request proves the real Git transport is running.
    // Keep its response withheld until shutdown has cancelled and reaped it.
    let (socket, _) = timeout(Duration::from_secs(10), transport.accept()).await??;
    let mut socket = BufReader::new(socket);
    let mut request = String::new();
    timeout(Duration::from_secs(10), async {
        loop {
            let bytes = socket.read_line(&mut request).await?;
            anyhow::ensure!(bytes != 0, "Git exited before sending its request");
            if request.ends_with("\r\n\r\n") {
                break Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    assert!(request.starts_with("GET /plugins.git/info/refs?service=git-upload-pack "));

    let status = timeout(Duration::from_secs(10), app.shutdown_gracefully()).await??;
    assert!(status.success(), "app-server shutdown failed: {status}");
    drop(app);
    // Check immediately: waiting for transport EOF first could hide a cleanup race.
    codex_home
        .close()
        .context("shutdown released the curated Git working directory")?;
    let mut remainder = Vec::new();
    let bytes = timeout(Duration::from_secs(10), socket.read_to_end(&mut remainder)).await??;
    assert_eq!(
        bytes, 0,
        "the blocked transport must close without a response"
    );
    Ok(())
}
