use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ThreadId;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::McpEventStreams;

struct SlowTaskDrop {
    active: Arc<AtomicUsize>,
    drop_started: Option<oneshot::Sender<()>>,
}

impl Drop for SlowTaskDrop {
    fn drop(&mut self) {
        if let Some(drop_started) = self.drop_started.take() {
            let _ = drop_started.send(());
        }
        std::thread::sleep(Duration::from_millis(100));
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

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
            move |_cancellation| {
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
        .start_task("shared".to_string(), thread_id, |_cancellation| {
            tokio::spawn(async {})
        })
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
                .is_some_and(super::McpEventStreamTask::is_finished);
            if finished {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal write should finish");

    streams
        .start_task("shared".to_string(), thread_id, |_cancellation| {
            tokio::spawn(async {})
        })
        .await
        .expect("same-ID restart should succeed after terminal write completes");
    streams.clear().await;
}

#[tokio::test]
async fn explicit_stop_keeps_subscription_leased_until_in_flight_write_completes() {
    let streams = Arc::new(McpEventStreams::default());
    let thread_id = ThreadId::new();
    let write_started = Arc::new(Notify::new());
    let write_completed = Arc::new(Notify::new());

    streams
        .start_task("shared".to_string(), thread_id, {
            let write_started = Arc::clone(&write_started);
            let write_completed = Arc::clone(&write_completed);
            move |_cancellation| {
                tokio::spawn(async move {
                    write_started.notify_one();
                    write_completed.notified().await;
                })
            }
        })
        .await
        .expect("first subscription should start");
    write_started.notified().await;

    let stop = tokio::spawn({
        let streams = Arc::clone(&streams);
        async move { streams.stop("shared").await }
    });
    loop {
        let stopping = streams
            .tasks
            .lock()
            .await
            .get("shared")
            .is_some_and(|task| {
                matches!(&task.state, super::McpEventStreamTaskState::Stopping { .. })
            });
        if stopping {
            break;
        }
        tokio::task::yield_now().await;
    }
    let replacement = tokio::spawn({
        let streams = Arc::clone(&streams);
        async move {
            streams
                .start_task("shared".to_string(), thread_id, |_cancellation| {
                    tokio::spawn(async {})
                })
                .await
        }
    });

    assert!(
        timeout(Duration::from_millis(50), async {
            while !stop.is_finished() || !replacement.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "stop and same-ID replacement must wait for the queued write acknowledgement"
    );
    write_completed.notify_one();
    stop.await.expect("stop task should join");
    replacement
        .await
        .expect("replacement task should join")
        .expect("replacement should start after the physical write completes");
    streams.clear().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_id_remains_leased_until_explicit_stop_joins_task() {
    let streams = Arc::new(McpEventStreams::default());
    let thread_id = ThreadId::new();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum_active = Arc::new(AtomicUsize::new(0));
    let (drop_started_tx, drop_started_rx) = oneshot::channel();
    streams
        .start_task("shared".to_string(), thread_id, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move |cancellation| {
                tokio::spawn(async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum_active.fetch_max(current, Ordering::AcqRel);
                    let _drop = SlowTaskDrop {
                        active,
                        drop_started: Some(drop_started_tx),
                    };
                    cancellation.cancelled().await;
                })
            }
        })
        .await
        .expect("first subscription should start");

    let stopping_streams = Arc::clone(&streams);
    let stop = tokio::spawn(async move { stopping_streams.stop("shared").await });
    drop_started_rx
        .await
        .expect("aborted task should begin dropping");
    let starting_streams = Arc::clone(&streams);
    let replacement_active = Arc::clone(&active);
    let replacement_maximum_active = Arc::clone(&maximum_active);
    let replacement = tokio::spawn(async move {
        starting_streams
            .start_task("shared".to_string(), thread_id, {
                let active = replacement_active;
                let maximum_active = replacement_maximum_active;
                move |cancellation| {
                    tokio::spawn(async move {
                        let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                        maximum_active.fetch_max(current, Ordering::AcqRel);
                        cancellation.cancelled().await;
                    })
                }
            })
            .await
    });
    stop.await.expect("stop task should join");
    replacement
        .await
        .expect("replacement task should join")
        .expect("replacement should start after stop releases the ID");

    assert_eq!(maximum_active.load(Ordering::Acquire), 1);
    streams.clear().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_stop_waiter_keeps_subscription_id_leased() {
    let streams = Arc::new(McpEventStreams::default());
    let thread_id = ThreadId::new();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum_active = Arc::new(AtomicUsize::new(0));
    let (drop_started_tx, drop_started_rx) = oneshot::channel();
    streams
        .start_task("shared".to_string(), thread_id, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move |cancellation| {
                tokio::spawn(async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum_active.fetch_max(current, Ordering::AcqRel);
                    let _drop = SlowTaskDrop {
                        active,
                        drop_started: Some(drop_started_tx),
                    };
                    cancellation.cancelled().await;
                })
            }
        })
        .await
        .expect("first subscription should start");

    let stopping_streams = Arc::clone(&streams);
    let stop = tokio::spawn(async move { stopping_streams.stop("shared").await });
    drop_started_rx
        .await
        .expect("aborted task should begin dropping");
    stop.abort();

    streams
        .start_task("shared".to_string(), thread_id, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move |cancellation| {
                tokio::spawn(async move {
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    maximum_active.fetch_max(current, Ordering::AcqRel);
                    cancellation.cancelled().await;
                })
            }
        })
        .await
        .expect("replacement should start after detached cleanup releases the ID");

    assert_eq!(maximum_active.load(Ordering::Acquire), 1);
    streams.clear().await;
}
