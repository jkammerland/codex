use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use rmcp::model::CustomNotification;

use super::event_notification_channel;

#[test]
fn closing_receiver_makes_a_concurrent_drain_finite() {
    let (sender, mut receiver) = event_notification_channel();
    let producer_stopped = Arc::new(AtomicBool::new(false));
    let producer_stopped_for_thread = Arc::clone(&producer_stopped);
    let producer = std::thread::spawn(move || {
        while sender
            .send(
                CustomNotification::new("notifications/events/event", /*params*/ None),
                /*notification_bytes*/ 1,
            )
            .is_ok()
        {}
        producer_stopped_for_thread.store(true, Ordering::Release);
    });

    while receiver.try_recv().is_err() {
        std::thread::yield_now();
    }
    receiver.close();
    let buffered = std::iter::from_fn(|| receiver.try_recv().ok()).count();

    producer
        .join()
        .expect("producer must stop after receiver closes");
    assert!(producer_stopped.load(Ordering::Acquire));
    assert!(buffered <= super::MAX_QUEUED_EVENT_NOTIFICATION_BYTES);
}
