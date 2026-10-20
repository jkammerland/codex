use super::CuratedGitLifecycle;
use super::sync_openai_plugins_repo_with_transport_overrides;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::any;

#[tokio::test]
async fn cancelling_active_git_does_not_start_http_fallback() -> anyhow::Result<()> {
    const PROBE_ADDRESS: &str = "CODEX_TEST_CURATED_SHUTDOWN_PROBE";
    const TEST_NAME: &str =
        "startup_sync::shutdown_tests::cancelling_active_git_does_not_start_http_fallback";
    if let Some(address) = std::env::var_os(PROBE_ADDRESS) {
        // The Git stand-in blocks on a socket; no timers or external services race shutdown.
        let mut socket = std::net::TcpStream::connect(address.to_string_lossy().as_ref())?;
        socket.write_all(b"ready")?;
        let mut input = [0];
        socket.read_exact(&mut input)?;
        anyhow::bail!("blocked Git probe should have been terminated");
    }

    let fixture = tempfile::tempdir()?;
    let codex_home = tempfile::tempdir()?;
    let probe = TcpListener::bind("127.0.0.1:0").await?;
    let executable = std::env::current_exe()?;
    let quoted_executable = executable.to_string_lossy().replace('\'', "'\"'\"'");
    let git = fixture.path().join("git-probe");
    codex_utils_cargo_bin::write_executable(
        &git,
        &format!(
            "#!/bin/sh\n{PROBE_ADDRESS}='{}' exec '{quoted_executable}' --exact {TEST_NAME} --nocapture\n",
            probe.local_addr()?,
        ),
    )?;
    let fallback = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&fallback)
        .await;
    let lifecycle = Arc::new(CuratedGitLifecycle::default());
    let worker_lifecycle = Arc::clone(&lifecycle);
    let home = codex_home.path().to_path_buf();
    let fallback_url = fallback.uri();
    let worker = tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            &home,
            Some(&git),
            &fallback_url,
            &fallback_url,
            &crate::test_support::test_http_client_factory(),
            &worker_lifecycle,
        )
    });

    let (mut socket, _) = timeout(Duration::from_secs(10), probe.accept()).await??;
    let mut ready = [0; 5];
    timeout(Duration::from_secs(10), socket.read_exact(&mut ready)).await??;
    assert_eq!(&ready, b"ready");
    timeout(Duration::from_secs(10), lifecycle.shutdown()).await?;
    let result = timeout(Duration::from_secs(10), worker).await??;
    assert_eq!(
        result,
        Err("curated plugin sync cancelled during shutdown".to_string())
    );
    let mut remaining = Vec::new();
    assert_eq!(socket.read_to_end(&mut remaining).await?, 0);
    assert!(
        fallback
            .received_requests()
            .await
            .expect("record requests")
            .is_empty()
    );
    assert!(!codex_home.path().join(".tmp/plugins.sha").exists());
    codex_home.close()?;
    Ok(())
}
