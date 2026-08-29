use super::CHANNEL_CAPACITY;
use super::ConnectionOrigin;
use super::TransportEvent;
use super::forward_incoming_message;
use super::next_connection_id;
use super::serialize_outgoing_message;
use crate::outgoing_message::QueuedOutgoingMessage;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCRequest;
use std::io::BufRead as _;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use tokio::io;
#[cfg(test)]
use tokio::io::AsyncBufReadExt;
#[cfg(test)]
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
#[cfg(test)]
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error;
use tracing::info;

pub async fn start_stdio_connection(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
    initialize_client_name_tx: oneshot::Sender<String>,
) -> IoResult<()> {
    start_stdio_connection_with_shutdown(
        transport_event_tx,
        stdio_handles,
        initialize_client_name_tx,
        CancellationToken::new(),
    )
    .await
}

pub async fn start_stdio_connection_with_shutdown(
    transport_event_tx: mpsc::Sender<TransportEvent>,
    stdio_handles: &mut Vec<JoinHandle<()>>,
    initialize_client_name_tx: oneshot::Sender<String>,
    shutdown: CancellationToken,
) -> IoResult<()> {
    let connection_id = next_connection_id();
    let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(CHANNEL_CAPACITY);
    let writer_tx_for_reader = writer_tx.clone();
    transport_event_tx
        .send(TransportEvent::ConnectionOpened {
            connection_id,
            origin: ConnectionOrigin::Stdio,
            writer: writer_tx,
            disconnect_sender: None,
        })
        .await
        .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "processor unavailable"))?;

    let (stdin_tx, stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
    spawn_stdin_reader(stdin_tx)?;
    let transport_event_tx_for_reader = transport_event_tx.clone();
    stdio_handles.push(tokio::spawn(async move {
        run_stdio_channel_reader(
            stdin_rx,
            transport_event_tx_for_reader,
            writer_tx_for_reader,
            connection_id,
            initialize_client_name_tx,
            shutdown,
        )
        .await;
    }));

    stdio_handles.push(tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(queued_message) = writer_rx.recv().await {
            let Some(mut json) = serialize_outgoing_message(queued_message.message) else {
                continue;
            };
            json.push('\n');
            if let Err(err) = stdout.write_all(json.as_bytes()).await {
                error!("Failed to write to stdout: {err}");
                break;
            }
            if let Some(write_complete_tx) = queued_message.write_complete_tx {
                let _ = write_complete_tx.send(());
            }
        }
        info!("stdout writer exited (channel closed)");
    }));

    Ok(())
}

enum StdioReadResult {
    Line(String),
    Eof,
    Error(std::io::Error),
}

fn spawn_stdin_reader(sender: mpsc::Sender<StdioReadResult>) -> IoResult<()> {
    // Tokio's stdin uses its blocking pool and cannot cancel an outstanding OS read. A detached
    // process-lifetime thread keeps that read outside the async runtime, so runtime shutdown never
    // waits for idle stdin. The operating system tears the thread down when the process exits.
    std::thread::Builder::new()
        .name("codex-app-server-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut line = String::new();
                let result = match stdin.read_line(&mut line) {
                    Ok(0) => StdioReadResult::Eof,
                    Ok(_) => {
                        if line.ends_with('\n') {
                            line.pop();
                            if line.ends_with('\r') {
                                line.pop();
                            }
                        }
                        StdioReadResult::Line(line)
                    }
                    Err(err) => StdioReadResult::Error(err),
                };
                let terminal = !matches!(result, StdioReadResult::Line(_));
                if sender.blocking_send(result).is_err() || terminal {
                    break;
                }
            }
        })
        .map(drop)
}

async fn run_stdio_channel_reader(
    mut stdin_rx: mpsc::Receiver<StdioReadResult>,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    writer_tx: mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: super::ConnectionId,
    initialize_client_name_tx: oneshot::Sender<String>,
    shutdown: CancellationToken,
) {
    let mut initialize_client_name_tx = Some(initialize_client_name_tx);
    loop {
        let read_result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            read_result = stdin_rx.recv() => read_result,
        };
        match read_result {
            Some(StdioReadResult::Line(line)) => {
                if !forward_stdio_line(
                    &line,
                    &transport_event_tx,
                    &writer_tx,
                    connection_id,
                    &mut initialize_client_name_tx,
                )
                .await
                {
                    break;
                }
            }
            Some(StdioReadResult::Error(err)) => {
                error!("Failed reading stdin: {err}");
                break;
            }
            Some(StdioReadResult::Eof) | None => break,
        }
    }

    let _ = transport_event_tx
        .send(TransportEvent::ConnectionClosed { connection_id })
        .await;
    debug!("stdin reader finished (EOF or shutdown)");
}

#[cfg(test)]
async fn run_stdio_reader(
    reader: impl AsyncRead + Unpin,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    writer_tx: mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: super::ConnectionId,
    initialize_client_name_tx: oneshot::Sender<String>,
    shutdown: CancellationToken,
) {
    let mut lines = BufReader::new(reader).lines();
    let mut initialize_client_name_tx = Some(initialize_client_name_tx);

    loop {
        let next_line = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            next_line = lines.next_line() => next_line,
        };
        match next_line {
            Ok(Some(line)) => {
                if !forward_stdio_line(
                    &line,
                    &transport_event_tx,
                    &writer_tx,
                    connection_id,
                    &mut initialize_client_name_tx,
                )
                .await
                {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                error!("Failed reading stdin: {err}");
                break;
            }
        }
    }

    let _ = transport_event_tx
        .send(TransportEvent::ConnectionClosed { connection_id })
        .await;
    debug!("stdin reader finished (EOF)");
}

async fn forward_stdio_line(
    line: &str,
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    writer_tx: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: super::ConnectionId,
    initialize_client_name_tx: &mut Option<oneshot::Sender<String>>,
) -> bool {
    if let Some(client_name) = stdio_initialize_client_name(line)
        && let Some(initialize_client_name_tx) = initialize_client_name_tx.take()
    {
        let _ = initialize_client_name_tx.send(client_name);
    }
    forward_incoming_message(transport_event_tx, writer_tx, connection_id, line).await
}

fn stdio_initialize_client_name(line: &str) -> Option<String> {
    let message = serde_json::from_str::<JSONRPCMessage>(line).ok()?;
    let JSONRPCMessage::Request(JSONRPCRequest { method, params, .. }) = message else {
        return None;
    };
    if method != "initialize" {
        return None;
    }
    let params = serde_json::from_value::<InitializeParams>(params?).ok()?;
    Some(params.client_info.name)
}

#[cfg(test)]
#[path = "stdio_tests.rs"]
mod tests;
