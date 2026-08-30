use std::sync::Arc;
use std::time::Duration;

use codex_protocol::ThreadId;
use tokio::sync::Notify;
use tokio::time::timeout;

use super::McpEventStreams;

#[tokio::test]
async fn subscription_id_remains_leased_during_terminal_write() {
    let streams = McpEventStreams::default();
    let thread_id = ThreadId::new();
    let terminal_write_started = Arc::new(Notify::new());
    let terminal_write_completed = Arc::new(Notify::new());

    streams
        .start_task("shared".to_string(), thread_id, {
            let terminal_write_started = Arc::clone(&terminal_write_started);
            let terminal_write_completed = Arc::clone(&terminal_write_completed);
            move || {
                tokio::spawn(async move {
                    terminal_write_started.notify_one();
                    terminal_write_completed.notified().await;
                })
            }
        })
        .await
        .expect("first subscription should reserve its ID");
    terminal_write_started.notified().await;

    let error = streams
        .start_task("shared".to_string(), thread_id, || tokio::spawn(async {}))
        .await
        .expect_err("same-ID restart must fail while terminal write is pending");
    assert!(error.message.contains("already exists"));

    terminal_write_completed.notify_one();
    timeout(Duration::from_secs(5), async {
        loop {
            let finished = streams
                .tasks
                .lock()
                .await
                .get("shared")
                .is_some_and(|task| task.task.is_finished());
            if finished {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal write should finish");

    streams
        .start_task("shared".to_string(), thread_id, || tokio::spawn(async {}))
        .await
        .expect("same-ID restart should succeed after terminal write completes");
    streams.clear().await;
}
