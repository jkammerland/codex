use super::*;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn shutdown_interrupts_idle_stdio_reader() {
    let (_input_tx, input_rx) = tokio::io::duplex(64);
    let (transport_event_tx, mut transport_event_rx) = mpsc::channel(4);
    let (writer_tx, _writer_rx) = mpsc::channel(4);
    let (initialize_client_name_tx, _initialize_client_name_rx) = oneshot::channel();
    let connection_id = next_connection_id();
    let shutdown = CancellationToken::new();
    let reader = tokio::spawn(run_stdio_reader(
        input_rx,
        transport_event_tx,
        writer_tx,
        connection_id,
        initialize_client_name_tx,
        shutdown.clone(),
    ));

    shutdown.cancel();
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("idle reader should observe shutdown")
        .expect("idle reader task should join");
    let event = transport_event_rx
        .recv()
        .await
        .expect("reader shutdown should close its connection");
    assert!(matches!(
        event,
        TransportEvent::ConnectionClosed {
            connection_id: closed
        } if closed == connection_id
    ));
}

#[tokio::test]
async fn shutdown_interrupts_channel_reader_while_blocking_stdin_owner_remains_alive() {
    let (_stdin_tx, stdin_rx) = mpsc::channel(1);
    let (transport_event_tx, mut transport_event_rx) = mpsc::channel(4);
    let (writer_tx, _writer_rx) = mpsc::channel(4);
    let (initialize_client_name_tx, _initialize_client_name_rx) = oneshot::channel();
    let connection_id = next_connection_id();
    let shutdown = CancellationToken::new();
    let reader = tokio::spawn(run_stdio_channel_reader(
        stdin_rx,
        transport_event_tx,
        writer_tx,
        connection_id,
        initialize_client_name_tx,
        shutdown.clone(),
    ));

    shutdown.cancel();
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("channel reader should not wait for the blocking stdin owner")
        .expect("channel reader task should join");
    let event = transport_event_rx
        .recv()
        .await
        .expect("channel reader shutdown should close its connection");
    assert!(matches!(
        event,
        TransportEvent::ConnectionClosed {
            connection_id: closed
        } if closed == connection_id
    ));
}
