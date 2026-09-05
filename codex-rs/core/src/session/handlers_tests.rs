use super::LiveBackgroundTerminalCleanup;
use super::submission_loop_with_background_terminal_cleanup;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::Submission;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::time::Duration;

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

struct AtomicDropSignal(Arc<AtomicBool>);

impl Drop for AtomicDropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn cancelling_live_cleanup_unblocks_following_shutdown_work() {
    let mut cleanup = LiveBackgroundTerminalCleanup::default();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    cleanup.start(async move {
        let _drop_signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    started_rx.await.expect("live cleanup should start");

    cleanup.cancel_and_wait().await;
    dropped_rx
        .await
        .expect("live cleanup resources must drop before shutdown continues");
}

#[tokio::test]
async fn queued_shutdown_preempts_live_cleanup_in_submission_loop() {
    let (session, _, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let config = session.get_config().await;
    let (tx_sub, rx_sub) = async_channel::unbounded();
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_dropped = Arc::new(AtomicBool::new(false));
    let cleanup_started_for_loop = Arc::clone(&cleanup_started);
    let cleanup_dropped_for_loop = Arc::clone(&cleanup_dropped);
    let loop_task = tokio::spawn(submission_loop_with_background_terminal_cleanup(
        Arc::clone(&session),
        config,
        rx_sub,
        move |_session| {
            let cleanup_started = Arc::clone(&cleanup_started_for_loop);
            let cleanup_dropped = Arc::clone(&cleanup_dropped_for_loop);
            async move {
                let _drop_signal = AtomicDropSignal(cleanup_dropped);
                cleanup_started.notify_one();
                std::future::pending::<()>().await;
            }
        },
    ));

    tx_sub
        .send(Submission {
            id: "clean".to_string(),
            op: Op::CleanBackgroundTerminals,
            trace: None,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await
        .expect("submit live cleanup");
    cleanup_started.notified().await;
    tx_sub
        .send(Submission {
            id: "shutdown".to_string(),
            op: Op::Shutdown,
            trace: None,
            parent_turn_id: None,
            root_turn_id: None,
        })
        .await
        .expect("queue shutdown behind live cleanup");

    tokio::time::timeout(Duration::from_secs(5), loop_task)
        .await
        .expect("queued shutdown should reach bounded session teardown")
        .expect("submission loop should not panic");
    assert!(
        cleanup_dropped.load(Ordering::Acquire),
        "live cleanup must be dropped before shutdown completes"
    );
    let shutdown_event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event
                .recv()
                .await
                .expect("event channel should remain open");
            if matches!(&event.msg, EventMsg::ShutdownComplete) {
                break event;
            }
        }
    })
    .await
    .expect("real shutdown should emit its completion event");
    assert_eq!(shutdown_event.id, "shutdown");
}
