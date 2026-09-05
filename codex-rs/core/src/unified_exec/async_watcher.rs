use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Sleep;

use super::SharedPluginMetricsSidecar;
use super::UnifiedExecContext;
use super::pending_launch::ManagerTaskHandle;
use super::pending_launch::PendingLaunch;
use super::pending_launch::PendingLaunchEventVisibility;
use super::process::OutputHandles;
use super::process::PublishedOutputChunk;
use super::process::UnifiedExecProcess;
use super::take_plugin_metrics_sidecar;
use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
use crate::plugins::metrics::finish_and_track_measurements;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_core_plugins::PluginCommandAttribution;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_utils_path_uri::PathUri;

pub(crate) const TRAILING_OUTPUT_GRACE: Duration = Duration::from_millis(100);

/// Upper bound for a single ExecCommandOutputDelta chunk emitted by unified exec.
///
/// The unified exec output buffer already caps *retained* output (see
/// `UNIFIED_EXEC_OUTPUT_MAX_BYTES`), but we also cap per-event payload size so
/// downstream event consumers (especially app-server JSON-RPC) don't have to
/// process arbitrarily large delta payloads.
const UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES: usize = 8192;

struct Emitter {
    remaining_deltas: usize,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    #[cfg(test)]
    emit_barrier: Option<EmitBarrier>,
}

#[cfg(test)]
struct EmitBarrier {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct Buffer<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES> {
    pending: Vec<u8>,
    deferred: HeadTailBuffer,
    event_visibility: PendingLaunchEventVisibility,
    emitter: Emitter,
}

/// Spawn a background task that continuously reads from the PTY and appends to
/// the shared transcript. Output deltas are bounded and withheld until the
/// manager confirms that the canonical Begin event has committed.
pub(crate) async fn start_streaming_output(
    process: &UnifiedExecProcess,
    context: &UnifiedExecContext,
    pending_launch: Arc<PendingLaunch>,
) {
    let mut event_visibility = pending_launch.subscribe_event_visibility();
    // Authoritative transcript publication and broadcast happen under the same
    // lock. Subscribe while holding it and seed replay from everything
    // published earlier, closing the fast-exit gap without replaying a byte
    // twice.
    let transcript = process.output_transcript();
    let (buffered_output, mut observed_bytes, mut receiver) = {
        let buffered_output = transcript.lock().await;
        let receiver = process.output_receiver();
        (
            buffered_output.clone(),
            buffered_output.total_bytes(),
            receiver,
        )
    };
    let output_drained = process.output_drained_notify();
    let exit_token = process.cancellation_token();
    let OutputHandles {
        output_closed,
        output_closed_notify,
        ..
    } = process.output_handles().clone();

    let emitter = Emitter {
        remaining_deltas: MAX_EXEC_OUTPUT_DELTAS_PER_CALL,
        session: Arc::clone(&context.session),
        turn: Arc::clone(&context.step_context.turn),
        call_id: context.call_id.clone(),
        #[cfg(test)]
        emit_barrier: None,
    };

    pending_launch.spawn_manager_task(async move {
        use tokio::sync::broadcast::error::RecvError;

        let mut output: Buffer = Buffer {
            pending: Vec::new(),
            deferred: HeadTailBuffer::default(),
            event_visibility: *event_visibility.borrow_and_update(),
            emitter,
        };
        output.replay_buffered(buffered_output).await;

        let mut grace_sleep: Option<Pin<Box<Sleep>>> = None;
        let output_closed_notified = output_closed_notify.notified();
        tokio::pin!(output_closed_notified);
        let mut output_complete = false;

        loop {
            // Register before checking the atomic so a close between the check
            // and the select cannot miss the notification.
            output_closed_notified.as_mut().enable();
            if grace_sleep.is_some() && output_closed.load(Ordering::Acquire) {
                output_complete = true;
                break;
            }

            tokio::select! {
                _ = exit_token.cancelled(), if grace_sleep.is_none() => {
                    let deadline = Instant::now() + TRAILING_OUTPUT_GRACE;
                    grace_sleep.replace(Box::pin(tokio::time::sleep_until(deadline)));
                }

                _ = async {
                    if let Some(sleep) = grace_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if grace_sleep.is_some() => {
                    break;
                }

                _ = &mut output_closed_notified, if grace_sleep.is_some() => {
                    output_closed_notified.set(output_closed_notify.notified());
                }

                changed = event_visibility.changed(), if output.events_pending() => {
                    let visibility = if changed.is_ok() {
                        *event_visibility.borrow_and_update()
                    } else {
                        PendingLaunchEventVisibility::Suppress
                    };
                    output.set_event_visibility(visibility).await;
                }

                received = receiver.recv() => {
                    let chunk = match received {
                        Ok(chunk) => chunk,
                        Err(RecvError::Lagged(_)) => {
                            output
                                .reconcile_authoritative(&transcript, &mut observed_bytes)
                                .await;
                            continue;
                        },
                        Err(RecvError::Closed) => {
                            output_complete = true;
                            break;
                        }
                    };

                    output
                        .push_published(chunk, &transcript, &mut observed_bytes)
                        .await;
                }
            }
        }

        output_complete |= output_closed.load(Ordering::Acquire);
        if output_complete {
            // Output producers publish all chunks before setting output_closed
            // with Release ordering, so the Acquire above makes this a final
            // safe drain.
            loop {
                let chunk = match receiver.try_recv() {
                    Ok(chunk) => chunk,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(
                        tokio::sync::broadcast::error::TryRecvError::Empty
                        | tokio::sync::broadcast::error::TryRecvError::Closed,
                    ) => break,
                };

                output
                    .push_published(chunk, &transcript, &mut observed_bytes)
                    .await;
            }
        }

        // A producer can outpace the bounded fan-out while replay publication
        // is awaiting the event sink. Reconcile from the producer-owned capped
        // transcript before declaring output drained.
        output
            .reconcile_authoritative(&transcript, &mut observed_bytes)
            .await;

        while output.events_pending() {
            let visibility = if event_visibility.changed().await.is_ok() {
                *event_visibility.borrow_and_update()
            } else {
                PendingLaunchEventVisibility::Suppress
            };
            output.set_event_visibility(visibility).await;
        }
        output
            .reconcile_authoritative(&transcript, &mut observed_bytes)
            .await;
        output.finish().await;
        output_drained.notify_one();
    });
}

/// Spawn a background watcher that waits for the PTY to exit and then emits a
/// single ExecCommandEnd event with the aggregated transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exit_watcher(
    process: Arc<UnifiedExecProcess>,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: i32,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    network_denial_monitor: Option<ManagerTaskHandle>,
    plugin_metrics_sidecar: Option<SharedPluginMetricsSidecar>,
) -> tokio::task::JoinHandle<()> {
    let exit_token = process.cancellation_token();
    let output_drained = process.output_drained_notify();
    let interaction_lock = process.interaction_lock();

    tokio::spawn(async move {
        exit_token.cancelled().await;
        output_drained.notified().await;
        // Deferred network denial deliberately remains observable for a short
        // window after process exit. Do not classify the terminal event until
        // that monitor has settled, even when output closes immediately.
        if let Some(network_denial_monitor) = network_denial_monitor {
            network_denial_monitor.wait().await;
        }
        let _interaction_guard = interaction_lock.lock_owned().await;

        let duration = Instant::now().saturating_duration_since(started_at);
        let plugin_metrics_sidecar = plugin_metrics_sidecar
            .as_ref()
            .and_then(take_plugin_metrics_sidecar);
        if let Some(message) = process.failure_message() {
            drop(plugin_metrics_sidecar);
            emit_failed_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                plugin_attribution,
                transcript,
                String::new(),
                message,
                duration,
            )
            .await;
        } else {
            let exit_code = process.exit_code().unwrap_or(-1);
            let timed_out = process.timed_out();
            finish_and_track_measurements(
                plugin_metrics_sidecar,
                exit_code,
                &session_ref,
                &turn_ref,
                &call_id,
            )
            .await;
            emit_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                plugin_attribution,
                transcript,
                String::new(),
                exit_code,
                duration,
                timed_out,
            )
            .await;
        }
    })
}

impl<const MAX_BYTES: usize> Buffer<MAX_BYTES> {
    fn events_pending(&self) -> bool {
        self.event_visibility == PendingLaunchEventVisibility::Pending
    }

    async fn set_event_visibility(&mut self, visibility: PendingLaunchEventVisibility) {
        if !self.events_pending() {
            return;
        }
        self.event_visibility = visibility;
        match visibility {
            PendingLaunchEventVisibility::Pending => {}
            PendingLaunchEventVisibility::Suppress => {
                self.deferred = HeadTailBuffer::default();
            }
            PendingLaunchEventVisibility::Emit => {
                let deferred = std::mem::take(&mut self.deferred);
                self.push_visible(deferred.to_bytes_with_omission_marker())
                    .await;
            }
        }
    }

    async fn replay_buffered(&mut self, buffered_output: HeadTailBuffer) {
        match self.event_visibility {
            PendingLaunchEventVisibility::Pending => self.deferred = buffered_output,
            PendingLaunchEventVisibility::Suppress => {}
            PendingLaunchEventVisibility::Emit => {
                self.push_visible(buffered_output.to_bytes_with_omission_marker())
                    .await;
            }
        }
    }

    async fn reconcile_authoritative(
        &mut self,
        transcript: &Arc<Mutex<HeadTailBuffer>>,
        observed_bytes: &mut usize,
    ) {
        let (total_bytes, deferred_replay, visible_replay) = {
            let authoritative = transcript.lock().await;
            let total_bytes = authoritative.total_bytes();
            let (deferred_replay, visible_replay) = match self.event_visibility {
                PendingLaunchEventVisibility::Pending => (Some(authoritative.clone()), None),
                PendingLaunchEventVisibility::Suppress => (None, None),
                PendingLaunchEventVisibility::Emit => (
                    None,
                    Some(authoritative.to_bytes_after_with_omission_marker(*observed_bytes)),
                ),
            };
            (total_bytes, deferred_replay, visible_replay)
        };
        if total_bytes <= *observed_bytes {
            return;
        }

        match self.event_visibility {
            PendingLaunchEventVisibility::Pending => {
                // No output has been made visible yet, so replacing the
                // deferred summary is both exact and duplicate-free.
                self.deferred = deferred_replay.unwrap_or_default();
            }
            PendingLaunchEventVisibility::Suppress => {}
            PendingLaunchEventVisibility::Emit => {
                self.push_visible(visible_replay.unwrap_or_default()).await;
            }
        }
        *observed_bytes = total_bytes;
    }

    async fn push_published(
        &mut self,
        chunk: PublishedOutputChunk,
        transcript: &Arc<Mutex<HeadTailBuffer>>,
        observed_bytes: &mut usize,
    ) {
        let end_offset = chunk.end_offset();
        if end_offset <= *observed_bytes {
            return;
        }
        if chunk.offset > *observed_bytes {
            self.reconcile_authoritative(transcript, observed_bytes)
                .await;
            if end_offset <= *observed_bytes {
                return;
            }
        }

        let skip = (*observed_bytes).saturating_sub(chunk.offset);
        let bytes = chunk.bytes.get(skip..).unwrap_or_default().to_vec();
        self.push(bytes).await;
        *observed_bytes = end_offset;
    }

    async fn push(&mut self, bytes: Vec<u8>) {
        const {
            assert!(
                MAX_BYTES >= char::MAX.len_utf8(),
                "a frame must fit one UTF-8 scalar"
            )
        };
        match self.event_visibility {
            PendingLaunchEventVisibility::Pending => self.deferred.push_chunk(&bytes),
            PendingLaunchEventVisibility::Suppress => {}
            PendingLaunchEventVisibility::Emit => self.push_visible(bytes).await,
        }
    }

    async fn push_visible(&mut self, mut bytes: Vec<u8>) {
        let Self {
            pending, emitter, ..
        } = self;

        // Reuse a producer chunk when it fits, retaining only an incomplete
        // UTF-8 suffix for the next push.
        if pending.is_empty() && bytes.len() <= MAX_BYTES {
            emitter
                .emit(|| {
                    let complete = utf8_boundary(&bytes);
                    pending.extend(bytes.drain(complete..));
                    bytes
                })
                .await;
            return;
        }

        let mut bytes = bytes.as_slice();
        let mut next_chunk = || {
            let space = MAX_BYTES.saturating_sub(pending.len());
            let (prefix, rest) = bytes.split_at_checked(space).unwrap_or((bytes, &[]));
            let mut chunk = Vec::with_capacity(pending.len().saturating_add(prefix.len()));
            chunk.append(pending); // Empties pending.
            chunk.extend_from_slice(prefix);
            bytes = rest;

            let complete = utf8_boundary(&chunk);
            // Only the incomplete suffix passes through pending.
            pending.extend(chunk.drain(complete..));
            chunk
        };
        while emitter.emit(&mut next_chunk).await {}
    }

    async fn finish(self) {
        let Self {
            pending,
            deferred: _,
            event_visibility,
            mut emitter,
        } = self;
        debug_assert!(
            pending.len() < char::MAX.len_utf8(),
            "only an incomplete UTF-8 scalar can remain"
        );
        if event_visibility == PendingLaunchEventVisibility::Emit {
            emitter.emit(|| pending).await;
        }
    }
}

impl Emitter {
    /// Build a frame only while quota remains. Returns whether a nonempty frame was sent.
    async fn emit(&mut self, make_chunk: impl FnOnce() -> Vec<u8>) -> bool {
        #[cfg(test)]
        if let Some(emit_barrier) = self.emit_barrier.take() {
            emit_barrier.entered.notify_one();
            emit_barrier.release.notified().await;
        }
        let Self {
            remaining_deltas,
            session,
            turn,
            call_id,
            #[cfg(test)]
                emit_barrier: _,
        } = self;
        let Some(remaining) = remaining_deltas.checked_sub(1) else {
            return false;
        };
        let chunk = make_chunk();
        let emit = !chunk.is_empty();
        if emit {
            let event = ExecCommandOutputDeltaEvent {
                call_id: call_id.clone(),
                stream: ExecOutputStream::Stdout,
                chunk,
            };
            session
                .send_event(turn.as_ref(), EventMsg::ExecCommandOutputDelta(event))
                .await;
            *remaining_deltas = remaining;
        }
        emit
    }
}

/// Emit an ExecCommandEnd event for a unified exec session, using the transcript
/// as the primary source of aggregated_output and falling back to the provided
/// text when the transcript is empty.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    exit_code: i32,
    duration: Duration,
    timed_out: bool,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        plugin_attribution,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Success {
                output,
                applied_patch_delta: None,
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_failed_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    process_id: Option<String>,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    duration: Duration,
) {
    let stdout = if fallback_output.is_empty() {
        resolve_aggregated_output(&transcript, fallback_output).await
    } else {
        fallback_output
    };
    let aggregated_output = if stdout.is_empty() {
        message.clone()
    } else {
        format!("{stdout}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(message),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        plugin_attribution,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Failure(ToolEventFailure::Output(output)),
        )
        .await;
}

/// Keep only a potentially incomplete UTF-8 suffix; malformed bytes remain raw.
/// A UTF-8 scalar spans at most four bytes, so its incomplete tail fits in three.
fn utf8_boundary(bytes: &[u8]) -> usize {
    let mut boundary = bytes.len().saturating_sub(char::MAX.len_utf8() - 1);
    while boundary < bytes.len() {
        match std::str::from_utf8(&bytes[boundary..]) {
            Ok(s) => return boundary + s.len(),
            Err(error) => {
                boundary += error.valid_up_to();
                if let Some(invalid_len) = error.error_len() {
                    boundary += invalid_len;
                } else {
                    return boundary;
                }
            }
        }
    }
    bytes.len()
}

async fn resolve_aggregated_output(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    let guard = transcript.lock().await;
    if guard.retained_bytes() == 0 {
        return fallback;
    }

    String::from_utf8_lossy(&guard.to_bytes_with_omission_marker()).to_string()
}

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
