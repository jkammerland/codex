use anyhow::Result;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn concurrent_discovery_reuses_the_authenticated_coordinator() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.coordinator.listen = listener.local_addr()?;
    manifest.coordinator.url = format!("http://{}", listener.local_addr()?);
    tokio::fs::write(&manifest.coordinator.token_file, "private-test-token").await?;
    let manifest_path = directory.path().join("agents.toml");
    tokio::fs::write(&manifest_path, toml::to_string(&manifest)?).await?;
    let coordinator = crate::coordinator::Coordinator::open(manifest).await?;
    let expected = coordinator.current_control().await?;
    let app = crate::http::router(coordinator, "private-test-token".into());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let missing_executable = directory.path().join("must-not-be-spawned");
    let (first, second) = tokio::try_join!(
        super::ensure_coordinator(&manifest_path, &missing_executable),
        super::ensure_coordinator(&manifest_path, &missing_executable),
    )?;
    assert_eq!(first.snapshot().await?.control, expected);
    assert_eq!(second.snapshot().await?.control, expected);

    // A fast probe may defer a partial credential read, but an actual server's
    // authentication rejection must remain an error instead of spawning again.
    tokio::fs::write(
        &crate::Manifest::load(&manifest_path)
            .await?
            .coordinator
            .token_file,
        "wrong-token",
    )
    .await?;
    let error = super::ensure_coordinator(&manifest_path, &missing_executable)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("the coordinator accepted an unrelated credential"))?;
    assert!(
        error
            .to_string()
            .contains("did not accept this manifest's credential")
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn automatic_children_serialize_partial_credentials_and_reuse_the_ready_owner() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.coordinator.listen = listener.local_addr()?;
    manifest.coordinator.url = format!("http://{}", listener.local_addr()?);

    // This guard belongs to the detached child, so retiring its original UI
    // does not release serialization during token creation or listener setup.
    let first_child = super::prepare_automatic_start(&manifest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("an unexpected coordinator already owns the fixture"))?;
    tokio::fs::write(&manifest.coordinator.token_file, "").await?;
    assert!(
        super::probe_existing(&manifest, /*allow_partial_credential*/ true)
            .await?
            .is_none()
    );
    let mut second_child = Box::pin(super::prepare_automatic_start(&manifest));
    assert!(futures::poll!(&mut second_child).is_pending());

    tokio::fs::write(&manifest.coordinator.token_file, "private-test-token").await?;
    let coordinator = crate::coordinator::Coordinator::open(manifest.clone()).await?;
    let app = crate::http::router(coordinator, "private-test-token".into());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    drop(first_child);

    // The waiting child observes the first child's authenticated listener and
    // exits as a readiness relay instead of racing token creation or binding.
    assert!(second_child.await?.is_none());
    server.abort();
    Ok(())
}

#[test]
fn canceled_startup_lock_wait_does_not_retain_the_tokio_runtime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let lock_path = directory.path().join("startup.lock");
    let held_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    held_lock.lock()?;

    let (finished, completion) = std::sync::mpsc::channel();
    let runtime_owner = std::thread::spawn(move || -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut startup = Box::pin(super::acquire_startup_lock(lock_path));
            // Poll once to start the native waiter, then cancel the UI owner
            // while another startup still holds the cross-process lock.
            assert!(futures::poll!(&mut startup).is_pending());
            drop(startup);
        });
        drop(runtime);
        finished.send(())?;
        Ok(())
    });

    // This deadline detects a runtime-shutdown deadlock in the regression;
    // production startup continues to wait on its lock without a deadline.
    let result = completion.recv_timeout(std::time::Duration::from_secs(10));
    held_lock.unlock()?;
    runtime_owner
        .join()
        .map_err(|_| anyhow::anyhow!("startup runtime owner panicked"))??;
    result
        .map_err(|error| anyhow::anyhow!("canceled startup retained the Tokio runtime: {error}"))?;
    Ok(())
}

#[test]
fn readiness_survives_a_closed_parent_pipe_and_preserves_other_io_errors() -> Result<()> {
    struct FailedWriter {
        kind: std::io::ErrorKind,
        fail_on_flush: bool,
    }

    impl std::io::Write for FailedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_on_flush {
                Ok(bytes.len())
            } else {
                Err(std::io::Error::from(self.kind))
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(self.kind))
        }
    }

    let mut output = Vec::new();
    super::report_ready(&mut output)?;
    assert_eq!(String::from_utf8(output)?, "CODEX_AGENTS_READY\n");
    for fail_on_flush in [false, true] {
        super::report_ready(&mut FailedWriter {
            kind: std::io::ErrorKind::BrokenPipe,
            fail_on_flush,
        })?;
        assert_eq!(
            super::report_ready(&mut FailedWriter {
                kind: std::io::ErrorKind::PermissionDenied,
                fail_on_flush
            })
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::PermissionDenied,
        );
    }
    Ok(())
}
