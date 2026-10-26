use super::*;
use crate::tools::context::ProcessWaitReason;
use crate::unified_exec::WaitProcessRequest;
use crate::unified_exec::async_watcher::TRAILING_OUTPUT_GRACE;

impl UnifiedExecProcessManager {
    pub(crate) async fn wait_process(
        &self,
        request: WaitProcessRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let WaitProcessRequest {
            terminal,
            max_output_tokens,
            truncation_policy,
            interaction_event,
            activity_rx,
            pending_activity,
        } = request;

        let process_id = terminal.process_id;
        let locked_process = &terminal.process;

        // A single event-driven waiter owns the next observable process event.
        // Reject duplicates instead of letting one drain the event and strand another.
        let _wait_process_guard = locked_process
            .wait_process_lock()
            .try_lock_owned()
            .map_err(|_| UnifiedExecError::WaitAlreadyActive { process_id })?;
        let interaction_guard = locked_process.interaction_lock().lock_owned().await;

        let PreparedProcessHandles {
            process,
            output,
            pause_state: _,
            session,
            network_approval,
            call_id,
            hook_command,
            process_id,
            tty,
        } = self
            .prepare_process_handles(process_id, locked_process)
            .await?;
        let start = Instant::now();
        let mut state_rx = process.subscribe_state();
        let has_buffered_output =
            tty && output.output_buffer.lock().await.pending.total_bytes() > 0;

        let initial_wait_reason = if process.failure_message().is_some()
            || (process.has_exited()
                && (output.drain_policy == OutputDrainPolicy::BoundedAfterExit
                    || output.output_closed.load(Ordering::Acquire)
                    || process.termination_requested.load(Ordering::Acquire)))
        {
            Some(ProcessWaitReason::Completed)
        } else if pending_activity.is_some() {
            Some(ProcessWaitReason::Input)
        } else if has_buffered_output {
            Some(ProcessWaitReason::Output)
        } else {
            None
        };
        let (wait_reason, _interaction_guard) = if let Some(wait_reason) = initial_wait_reason {
            (wait_reason, interaction_guard)
        } else {
            if let Some(interaction_event) = interaction_event.as_ref() {
                let interaction = TerminalInteractionEvent {
                    call_id: call_id.clone(),
                    process_id: process_id.to_string(),
                    stdin: String::new(),
                };
                interaction_event
                    .session
                    .send_event(
                        interaction_event.turn.as_ref(),
                        EventMsg::TerminalInteraction(interaction),
                    )
                    .await;
            }
            // The event listeners below protect the wait boundary; keeping the
            // interaction lock here would prevent stdin from waking the process.
            drop(interaction_guard);

            loop {
                // Register all event listeners before re-checking state so no
                // process, output, or input event can be lost in between.
                let state_changed = state_rx.changed();
                let activity_changed = activity_rx.changed();
                let output_notified = output.output_notify.notified();
                let output_closed = output.output_closed_notify.notified();
                tokio::pin!(state_changed);
                tokio::pin!(activity_changed);
                tokio::pin!(output_notified);
                tokio::pin!(output_closed);
                // `notify_waiters` does not retain a permit. Enable the waiter
                // before the buffer recheck so output arriving at this boundary
                // cannot be lost for an otherwise quiet interactive process.
                output_notified.as_mut().enable();
                output_closed.as_mut().enable();

                let interaction_guard = locked_process.interaction_lock().lock_owned().await;
                if process.failure_message().is_some()
                    || (process.has_exited()
                        && (output.drain_policy == OutputDrainPolicy::BoundedAfterExit
                            || output.output_closed.load(Ordering::Acquire)
                            || process.termination_requested.load(Ordering::Acquire)))
                {
                    break (ProcessWaitReason::Completed, interaction_guard);
                }
                if tty && output.output_buffer.lock().await.pending.total_bytes() > 0 {
                    break (ProcessWaitReason::Output, interaction_guard);
                }
                drop(interaction_guard);

                tokio::select! {
                    biased;
                    _ = &mut state_changed => {}
                    _ = &mut output_closed => {}
                    _ = &mut activity_changed => {
                        let interaction_guard =
                            locked_process.interaction_lock().lock_owned().await;
                        break (ProcessWaitReason::Input, interaction_guard);
                    },
                    _ = &mut output_notified, if tty => {
                        // The producer event remains meaningful even if a
                        // concurrent write drains its bytes before this waiter
                        // reacquires the interaction lock.
                        let interaction_guard =
                            locked_process.interaction_lock().lock_owned().await;
                        break (ProcessWaitReason::Output, interaction_guard);
                    }
                }
            }
        };

        // Remote completion requires output closure; local execution retains its
        // upstream bounded grace for descendants holding inherited pipes.
        let collection_deadline = if wait_reason == ProcessWaitReason::Completed {
            Instant::now() + TRAILING_OUTPUT_GRACE
        } else {
            Instant::now()
        };
        let collected_output = Self::collect_output_until_deadline(
            &output,
            /*pause_state*/ None,
            collection_deadline,
        )
        .await;
        let wall_time = Instant::now().saturating_duration_since(start);
        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let chunk_id = generate_chunk_id();

        if network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message =
                network_denial_message_for_session(session.as_ref(), network_approval.clone())
                    .await;
            self.release_process_id_if_matches(process_id, &process)
                .await;
            return Err(fail_process_with_message(process.as_ref(), message));
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                session.as_ref(),
                network_approval.clone(),
            )
            .await;
            self.release_process_id_if_matches(process_id, &process)
                .await;
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }

        let status = self
            .refresh_process_state(process_id, &process, OutputRetention::UntilCollected)
            .await;
        let (process_id, exit_code, event_call_id, wait_reason) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id, wait_reason),
            ProcessStatus::Exited { exit_code, entry } => {
                entry.process.cancellation_token().cancel();
                let call_id = entry.call_id.clone();
                if let Err(message) =
                    finish_network_approval_after_process_exit_for_entry(&entry).await
                {
                    return Err(fail_process_with_message(entry.process.as_ref(), message));
                }
                (None, exit_code, call_id, ProcessWaitReason::Completed)
            }
            ProcessStatus::Unknown => {
                if process.has_exited() {
                    (
                        None,
                        process.exit_code(),
                        call_id,
                        ProcessWaitReason::Completed,
                    )
                } else {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        };

        Ok(ExecCommandToolOutput {
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy,
            max_output_tokens,
            process_id,
            exit_code,
            wait_reason: Some(wait_reason),
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(hook_command),
        })
    }
}
