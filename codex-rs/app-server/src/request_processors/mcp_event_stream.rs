use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::McpServerEventNotification;
use codex_app_server_protocol::McpServerEventStreamNotification;
use codex_app_server_protocol::McpServerEventStreamStartParams;
use codex_app_server_protocol::ServerNotification;
use codex_core::CodexThread;
use codex_login::AuthManager;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpEventStream;
use codex_mcp::McpEventStreamEnd;
use codex_mcp::McpEventStreamItem;
use codex_protocol::ThreadId;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::mcp_processor::McpRequestProcessor;
use crate::error_code::OVERLOADED_ERROR_CODE;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;

const MAX_MCP_EVENT_STREAMS_PER_CONNECTION: usize = 64;
const MCP_EVENT_STREAM_STARTUP_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 90);
const MCP_EVENT_STREAM_STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);

pub(crate) type McpEventStreamReady = oneshot::Receiver<Result<(), JSONRPCErrorError>>;

#[derive(Debug)]
struct McpEventStreamTask {
    thread_id: ThreadId,
    state: McpEventStreamTaskState,
}

impl McpEventStreamTask {
    fn is_finished(&self) -> bool {
        match &self.state {
            McpEventStreamTaskState::Running { task, .. } => task.is_finished(),
            McpEventStreamTaskState::Stopping { .. } => false,
        }
    }
}

#[derive(Debug)]
enum McpEventStreamTaskState {
    Running {
        task: JoinHandle<()>,
        cancellation: CancellationToken,
    },
    Stopping {
        completed: watch::Receiver<bool>,
        lease: Arc<()>,
        abort: AbortHandle,
    },
}

#[derive(Debug, Default)]
pub(crate) struct McpEventStreams {
    tasks: Arc<Mutex<HashMap<String, McpEventStreamTask>>>,
}

impl McpEventStreams {
    pub(crate) async fn start(
        &self,
        connection_id: ConnectionId,
        params: McpServerEventStreamStartParams,
        processor: McpRequestProcessor,
    ) -> Result<McpEventStreamReady, JSONRPCErrorError> {
        if params.server != CODEX_APPS_MCP_SERVER_NAME {
            return Err(invalid_request(
                "MCP event subscriptions are only supported for hosted apps",
            ));
        }
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
        let subscription_id = params.subscription_id.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        self.start_task(subscription_id, thread_id, |cancellation| {
            tokio::spawn(async move {
                let mut auth_changes =
                    McpEventStreamAuthChanges::new(Arc::clone(&processor.auth_manager));
                let opened = tokio::select! {
                    _ = cancellation.cancelled() => {
                        let _ = ready_tx.send(Err(internal_error(
                            "MCP event stream stopped during startup",
                        )));
                        return;
                    }
                    () = auth_changes.changed() => Err(internal_error(
                        "MCP event subscription authentication changed during startup",
                    )),
                    result = async {
                        if !processor
                            .thread_state_manager
                            .subscribed_connection_ids(thread_id)
                            .await
                            .contains(&connection_id)
                        {
                            return Err(invalid_request(format!(
                                "connection is not subscribed to thread '{thread_id}'"
                            )));
                        }
                        let (_, thread) = processor.load_thread(&params.thread_id).await?;
                        let stream = thread
                            .start_mcp_event_stream(
                                &params.name,
                                params.arguments.clone(),
                                params.meta.clone(),
                            )
                            .await
                            .map_err(|error| internal_error(format!(
                                "failed to start MCP event stream for '{}': {error:#}",
                                params.server
                            )))?;
                        Ok((stream, thread))
                    } => result,
                };
                match opened {
                    Ok((stream, thread)) => {
                        forward_events(McpEventForwarder {
                            connection_id,
                            params: &params,
                            outgoing: processor.outgoing.as_ref(),
                            thread,
                            stream,
                            auth_changes,
                            cancellation,
                            ready: Some(ready_tx),
                        })
                        .await;
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
        })
        .await?;
        Ok(ready_rx)
    }

    async fn start_task(
        &self,
        subscription_id: String,
        thread_id: ThreadId,
        spawn_task: impl FnOnce(CancellationToken) -> JoinHandle<()>,
    ) -> Result<(), JSONRPCErrorError> {
        let mut spawn_task = Some(spawn_task);
        loop {
            let stopping = {
                let mut tasks = self.tasks.lock().await;
                // A subscription ID remains leased until its forwarding task has
                // finished sending the terminal notification. This prevents a
                // delayed terminal event from being attributed to a same-ID
                // replacement.
                tasks.retain(|_, task| !task.is_finished());
                if let Some(task) = tasks.get(&subscription_id) {
                    match &task.state {
                        McpEventStreamTaskState::Running { .. } => {
                            return Err(invalid_request(format!(
                                "MCP event subscription '{subscription_id}' already exists"
                            )));
                        }
                        McpEventStreamTaskState::Stopping { completed, .. } => {
                            Some(completed.clone())
                        }
                    }
                } else {
                    if tasks.len() >= MAX_MCP_EVENT_STREAMS_PER_CONNECTION {
                        return Err(JSONRPCErrorError {
                            code: OVERLOADED_ERROR_CODE,
                            message: format!(
                                "MCP event subscription limit of {MAX_MCP_EVENT_STREAMS_PER_CONNECTION} reached"
                            ),
                            data: None,
                        });
                    }
                    let Some(spawn_task) = spawn_task.take() else {
                        return Err(internal_error(
                            "MCP event subscription task was already started",
                        ));
                    };
                    let cancellation = CancellationToken::new();
                    let task = spawn_task(cancellation.clone());
                    tasks.insert(
                        subscription_id.clone(),
                        McpEventStreamTask {
                            thread_id,
                            state: McpEventStreamTaskState::Running { task, cancellation },
                        },
                    );
                    None
                }
            };
            let Some(mut stopping) = stopping else {
                return Ok(());
            };
            if !*stopping.borrow()
                && tokio::time::timeout(MCP_EVENT_STREAM_STOP_WAIT_TIMEOUT, stopping.changed())
                    .await
                    .is_err()
            {
                return Err(internal_error(format!(
                    "MCP event subscription '{subscription_id}' is still stopping"
                )));
            }
        }
    }

    pub(crate) async fn wait_for_activation(
        &self,
        subscription_id: &str,
        ready: McpEventStreamReady,
    ) -> Result<(), JSONRPCErrorError> {
        match tokio::time::timeout(MCP_EVENT_STREAM_STARTUP_TIMEOUT, ready).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(internal_error(
                "MCP event stream ended before becoming active",
            )),
            Err(_) => {
                self.stop(subscription_id).await;
                Err(internal_error("MCP event stream startup timed out"))
            }
        }
    }

    pub(crate) async fn stop(&self, subscription_id: &str) {
        self.stop_where(|id, _| id == subscription_id).await;
    }

    pub(crate) async fn stop_thread(&self, thread_id: ThreadId) {
        self.stop_where(|_, task| task.thread_id == thread_id).await;
    }

    #[cfg(test)]
    pub(crate) async fn clear(&self) {
        self.stop_where(|_, _| true).await;
    }

    /// Aborts all forwarding tasks for a connection that is already known to be closed.
    /// Unlike explicit stop, no subscription ID can be reused on that connection, so retaining
    /// leases until physical write acknowledgement would only delay cleanup.
    pub(crate) async fn abort_for_connection_shutdown(&self) {
        let tasks = {
            let mut tasks = self.tasks.lock().await;
            tasks.drain().map(|(_, task)| task).collect::<Vec<_>>()
        };
        for task in tasks {
            match task.state {
                McpEventStreamTaskState::Running { task, cancellation } => {
                    cancellation.cancel();
                    task.abort();
                }
                McpEventStreamTaskState::Stopping { abort, .. } => abort.abort(),
            }
        }
    }

    async fn stop_where(&self, matches: impl Fn(&str, &McpEventStreamTask) -> bool) {
        let mut completions = Vec::new();
        {
            let mut tasks = self.tasks.lock().await;
            let matching_ids = tasks
                .iter()
                .filter(|(id, task)| matches(id, task))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for subscription_id in matching_ids {
                let Some(task) = tasks.get_mut(&subscription_id) else {
                    continue;
                };
                if let McpEventStreamTaskState::Stopping { completed, .. } = &task.state {
                    completions.push(completed.clone());
                    continue;
                }

                let (completed_tx, completed_rx) = watch::channel(false);
                let lease = Arc::new(());
                let abort = match &task.state {
                    McpEventStreamTaskState::Running { task, .. } => task.abort_handle(),
                    McpEventStreamTaskState::Stopping { abort, .. } => abort.clone(),
                };
                let previous = std::mem::replace(
                    &mut task.state,
                    McpEventStreamTaskState::Stopping {
                        completed: completed_rx.clone(),
                        lease: Arc::clone(&lease),
                        abort,
                    },
                );
                let McpEventStreamTaskState::Running { task, cancellation } = previous else {
                    continue;
                };
                cancellation.cancel();
                completions.push(completed_rx);

                let tasks = Arc::clone(&self.tasks);
                let _cleanup_task = tokio::spawn(async move {
                    let _ = task.await;
                    let mut tasks = tasks.lock().await;
                    let owns_lease =
                        tasks
                            .get(&subscription_id)
                            .is_some_and(|task| match &task.state {
                                McpEventStreamTaskState::Running { .. } => false,
                                McpEventStreamTaskState::Stopping {
                                    lease: current_lease,
                                    ..
                                } => Arc::ptr_eq(current_lease, &lease),
                            });
                    if owns_lease {
                        tasks.remove(&subscription_id);
                    }
                    drop(tasks);
                    let _ = completed_tx.send(true);
                });
            }
        }
        for mut completion in completions {
            if !*completion.borrow()
                && tokio::time::timeout(MCP_EVENT_STREAM_STOP_WAIT_TIMEOUT, completion.changed())
                    .await
                    .is_err()
            {
                tracing::warn!(
                    timeout_seconds = MCP_EVENT_STREAM_STOP_WAIT_TIMEOUT.as_secs(),
                    "timed out waiting for an MCP event stream write to finish"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "mcp_event_stream_tests.rs"]
mod tests;

struct McpEventForwarder<'a> {
    connection_id: ConnectionId,
    params: &'a McpServerEventStreamStartParams,
    outgoing: &'a OutgoingMessageSender,
    thread: Arc<CodexThread>,
    stream: McpEventStream,
    auth_changes: McpEventStreamAuthChanges,
    cancellation: CancellationToken,
    ready: Option<oneshot::Sender<Result<(), JSONRPCErrorError>>>,
}

async fn forward_events(forwarder: McpEventForwarder<'_>) {
    let McpEventForwarder {
        connection_id,
        params,
        outgoing,
        thread,
        mut stream,
        mut auth_changes,
        cancellation,
        mut ready,
    } = forwarder;
    let send = |notification| {
        outgoing.send_server_notification_to_connection_and_wait(
            connection_id,
            ServerNotification::McpServerEventStream(McpServerEventStreamNotification {
                subscription_id: params.subscription_id.clone(),
                notification,
            }),
        )
    };
    let termination_reason = loop {
        let notification = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            () = auth_changes.changed() => break "authenticationChanged",
            () = thread.wait_until_terminated() => break "threadTerminated",
            result = stream.recv_item() => match result {
                Ok(McpEventStreamItem::Notification(notification)) => notification,
                Ok(McpEventStreamItem::Ended(end_reason)) => {
                    if end_reason == McpEventStreamEnd::TransportClosed {
                        thread.request_mcp_transport_reconnect();
                    }
                    // Replaying a subscription without a server cursor can duplicate already
                    // delivered events or skip events emitted during recovery. Terminate this
                    // subscription and let the client explicitly choose whether to start anew.
                    break match end_reason {
                        McpEventStreamEnd::ServerEnded => "serverEnded",
                        McpEventStreamEnd::ConnectionSuperseded => "connectionSuperseded",
                        McpEventStreamEnd::TransportClosed => "transportClosed",
                    };
                }
                Err(error) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(internal_error(format!("{error:#}"))));
                    }
                    break "streamError";
                }
            },
        };
        let active = notification.method == "notifications/events/active";
        let terminated = notification.method == "notifications/events/terminated";
        if !send(McpServerEventNotification {
            method: notification.method,
            params: notification
                .params
                .unwrap_or_else(|| Value::Object(Default::default())),
        })
        .await
        {
            return;
        }
        if active && let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        if terminated {
            return;
        }
    };
    if let Some(ready) = ready {
        let _ = ready.send(Err(internal_error(format!(
            "MCP event stream ended before becoming active: {termination_reason}"
        ))));
    } else {
        if cancellation.is_cancelled() {
            return;
        }
        send(McpServerEventNotification {
            method: "notifications/events/terminated".to_string(),
            params: serde_json::json!({ "reason": termination_reason }),
        })
        .await;
    }
}

struct McpEventStreamAuthChanges {
    manager: Arc<AuthManager>,
    receiver: watch::Receiver<u64>,
    revision: u64,
    principal: Option<(Option<String>, Option<String>)>,
}

impl McpEventStreamAuthChanges {
    fn new(manager: Arc<AuthManager>) -> Self {
        let receiver = manager.auth_change_receiver();
        let revision = *receiver.borrow();
        let principal = manager
            .auth_cached()
            .map(|auth| (auth.get_account_id(), auth.get_chatgpt_user_id()));
        Self {
            manager,
            receiver,
            revision,
            principal,
        }
    }

    async fn changed(&mut self) {
        loop {
            if self
                .manager
                .auth_cached()
                .map(|auth| (auth.get_account_id(), auth.get_chatgpt_user_id()))
                != self.principal
            {
                return;
            }
            if self.receiver.changed().await.is_err() || self.principal == Some((None, None)) {
                return;
            }
            let revision = *self.receiver.borrow_and_update();
            if revision.wrapping_sub(self.revision) > 1 {
                return;
            }
            self.revision = revision;
        }
    }
}
