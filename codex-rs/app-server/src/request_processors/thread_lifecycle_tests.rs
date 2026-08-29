use super::*;
use futures::future;

#[tokio::test]
async fn ready_core_event_is_not_starved_by_listener_commands() {
    let (_cancel_tx, mut cancel_rx) = oneshot::channel();
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending_command = None;
    for _ in 0..=MAX_CONSECUTIVE_LISTENER_COMMANDS {
        listener_command_tx
            .send(ThreadListenerCommand::EmitThreadQueueChanged)
            .expect("listener command receiver should remain open");
    }

    for command_count in 0..MAX_CONSECUTIVE_LISTENER_COMMANDS {
        let input = next_thread_listener_input(
            &mut cancel_rx,
            &mut listener_command_rx,
            &mut pending_command,
            future::ready(()),
            future::pending(),
            if command_count >= MAX_CONSECUTIVE_LISTENER_COMMANDS {
                ThreadListenerPriority::FairEvent
            } else {
                ThreadListenerPriority::Commands
            },
        )
        .await;
        assert!(matches!(input, ThreadListenerInput::Command(Some(_))));
    }

    let input = next_thread_listener_input(
        &mut cancel_rx,
        &mut listener_command_rx,
        &mut pending_command,
        future::ready(()),
        future::pending(),
        ThreadListenerPriority::FairEvent,
    )
    .await;
    assert!(matches!(input, ThreadListenerInput::Event(())));
}

#[tokio::test]
async fn listener_cancellation_remains_highest_priority() {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending_command = None;
    listener_command_tx
        .send(ThreadListenerCommand::EmitThreadQueueChanged)
        .expect("listener command receiver should remain open");
    cancel_tx.send(()).expect("listener should remain open");

    let input = next_thread_listener_input(
        &mut cancel_rx,
        &mut listener_command_rx,
        &mut pending_command,
        future::ready(()),
        future::ready(true),
        ThreadListenerPriority::FairEvent,
    )
    .await;
    assert!(matches!(input, ThreadListenerInput::Cancelled));
}

#[tokio::test]
async fn transactional_listener_command_precedes_fair_event_then_makes_event_due() {
    let (_cancel_tx, mut cancel_rx) = oneshot::channel();
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending_command = None;
    let (completion_tx, _completion_rx) = oneshot::channel();
    listener_command_tx
        .send(ThreadListenerCommand::ResolveServerRequest {
            request_id: RequestId::Integer(7),
            completion_tx,
        })
        .expect("listener command receiver should remain open");

    let input = next_thread_listener_input(
        &mut cancel_rx,
        &mut listener_command_rx,
        &mut pending_command,
        future::ready(()),
        future::ready(true),
        ThreadListenerPriority::FairEvent,
    )
    .await;

    assert!(matches!(
        input,
        ThreadListenerInput::Command(Some(ThreadListenerCommand::ResolveServerRequest { .. }))
    ));

    let input = next_thread_listener_input(
        &mut cancel_rx,
        &mut listener_command_rx,
        &mut pending_command,
        future::ready(()),
        future::ready(true),
        ThreadListenerPriority::EventDue,
    )
    .await;
    assert!(matches!(input, ThreadListenerInput::Event(())));
}

#[tokio::test]
async fn event_stream_failure_closes_and_fails_transactional_commands() {
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolve_tx, resolve_rx) = oneshot::channel();
    let (seal_tx, seal_rx) = oneshot::channel();
    let mut pending_command = Some(ThreadListenerCommand::ResolveServerRequest {
        request_id: RequestId::Integer(7),
        completion_tx: resolve_tx,
    });
    listener_command_tx
        .send(ThreadListenerCommand::SealRealtimeUserInput {
            input: Vec::new(),
            completion_tx: seal_tx,
        })
        .expect("listener command receiver should remain open");
    let (outgoing_tx, _outgoing_rx) = tokio::sync::mpsc::channel(1);
    let outgoing = OutgoingMessageSender::new(
        outgoing_tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    );

    fail_pending_listener_commands(
        &mut pending_command,
        &mut listener_command_rx,
        &outgoing,
        "event stream failed",
    )
    .await;

    assert!(resolve_rx.await.is_err());
    assert_eq!(seal_rx.await, Ok(Err("event stream failed".to_string())));
    assert!(
        listener_command_tx
            .send(ThreadListenerCommand::EmitThreadQueueChanged)
            .is_err(),
        "failure cleanup should reject commands from stale sender clones"
    );
}
