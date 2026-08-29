use super::*;
use futures::future;

#[tokio::test]
async fn ready_core_event_is_not_starved_by_listener_commands() {
    let (_cancel_tx, mut cancel_rx) = oneshot::channel();
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..=MAX_CONSECUTIVE_LISTENER_COMMANDS {
        listener_command_tx
            .send(ThreadListenerCommand::EmitThreadQueueChanged)
            .expect("listener command receiver should remain open");
    }

    for command_count in 0..MAX_CONSECUTIVE_LISTENER_COMMANDS {
        let input = next_thread_listener_input(
            &mut cancel_rx,
            &mut listener_command_rx,
            future::ready(()),
            future::pending(),
            if command_count >= MAX_CONSECUTIVE_LISTENER_COMMANDS {
                ThreadListenerPriority::Events
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
        future::ready(()),
        future::pending(),
        ThreadListenerPriority::Events,
    )
    .await;
    assert!(matches!(input, ThreadListenerInput::Event(())));
}

#[tokio::test]
async fn listener_cancellation_remains_highest_priority() {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let (listener_command_tx, mut listener_command_rx) = tokio::sync::mpsc::unbounded_channel();
    listener_command_tx
        .send(ThreadListenerCommand::EmitThreadQueueChanged)
        .expect("listener command receiver should remain open");
    cancel_tx.send(()).expect("listener should remain open");

    let input = next_thread_listener_input(
        &mut cancel_rx,
        &mut listener_command_rx,
        future::ready(()),
        future::ready(true),
        ThreadListenerPriority::Events,
    )
    .await;
    assert!(matches!(input, ThreadListenerInput::Cancelled));
}
