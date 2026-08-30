use super::SPAWN_ROLLBACK_SHUTDOWN_TIMEOUT;
use super::UnpublishedSpawnShutdown;
use super::await_unpublished_spawn_shutdown;
use pretty_assertions::assert_eq;

#[tokio::test(start_paused = true)]
async fn unpublished_spawn_shutdown_is_bounded() {
    let shutdown = await_unpublished_spawn_shutdown(std::future::pending());
    tokio::pin!(shutdown);

    assert!(futures::poll!(&mut shutdown).is_pending());
    tokio::time::advance(SPAWN_ROLLBACK_SHUTDOWN_TIMEOUT).await;

    assert_eq!(shutdown.await, Ok(UnpublishedSpawnShutdown::TimedOut));
}
