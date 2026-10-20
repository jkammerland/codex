use super::CuratedGitLifecycle;
use futures::poll;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[tokio::test]
async fn shutdown_does_not_wait_for_started_publication() {
    let lifecycle = Arc::new(CuratedGitLifecycle::default());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let publisher_lifecycle = Arc::clone(&lifecycle);
    let publisher = tokio::task::spawn_blocking(move || {
        publisher_lifecycle.publish(|| {
            started_tx.send(()).expect("report publication started");
            release_rx.recv().expect("release filesystem cleanup");
            Ok(())
        })
    });
    started_rx.await.expect("publication reached its gate");
    assert_eq!(lifecycle.commands.len(), 0);

    // Isolate a regressed blocking lock from the test runtime so the watchdog
    // can release the publication and report a failure without leaving a thread.
    let runtime = tokio::runtime::Handle::current();
    let shutdown_lifecycle = Arc::clone(&lifecycle);
    let mut shutdown = tokio::task::spawn_blocking(move || {
        runtime.block_on(shutdown_lifecycle.shutdown());
    });
    let completed = timeout(Duration::from_secs(10), &mut shutdown).await;
    release_tx
        .send(())
        .expect("release publication after shutdown observation");
    publisher
        .await
        .expect("join publication")
        .expect("publication result");
    if completed.is_err() {
        shutdown
            .await
            .expect("join shutdown after releasing publication");
    }
    completed
        .expect("shutdown must finish while non-Git publication remains blocked")
        .expect("join shutdown");
    assert!(lifecycle.register().is_err());
}

#[tokio::test]
async fn shutdown_waits_for_command_cleanup_and_rejects_late_registration() {
    let lifecycle = Arc::new(CuratedGitLifecycle::default());
    let command = lifecycle.register().expect("register active command");
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let (cleaned_tx, cleaned_rx) = oneshot::channel();
    let worker_lifecycle = Arc::clone(&lifecycle);
    let worker = tokio::spawn(async move {
        worker_lifecycle.cancelled.cancelled().await;
        cancelled_tx.send(()).expect("report cancellation");
        cleaned_rx.await.expect("release process cleanup");
        drop(command);
    });

    let shutdown = lifecycle.shutdown();
    tokio::pin!(shutdown);
    assert!(poll!(&mut shutdown).is_pending());
    cancelled_rx.await.expect("command receives cancellation");
    assert!(lifecycle.register().is_err(), "late command must not start");
    let mut published = false;
    assert!(
        lifecycle
            .publish(|| {
                published = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!published, "shutdown must reject late publication");
    assert!(poll!(&mut shutdown).is_pending(), "cleanup is still owned");

    cleaned_tx.send(()).expect("complete process cleanup");
    worker.await.expect("join command cleanup");
    shutdown.await;
    assert_eq!(lifecycle.commands.len(), 0);
    assert!(lifecycle.register().is_err());
    lifecycle.shutdown().await;
}

#[tokio::test]
async fn shutdown_before_registration_does_not_launch_command() {
    let lifecycle = CuratedGitLifecycle::default();
    lifecycle.shutdown().await;
    // A nonexistent executable distinguishes cancellation from a spawn attempt.
    let error = lifecycle
        .run_git(
            std::process::Command::new("must-not-launch-curated-git"),
            "closed curated sync",
            std::time::Duration::from_secs(30),
        )
        .expect_err("closed lifecycle rejects the command");
    assert_eq!(error, "curated plugin sync cancelled during shutdown");
    assert_eq!(lifecycle.commands.len(), 0);
}
