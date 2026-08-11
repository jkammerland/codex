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
            process_id,
            max_output_tokens,
            truncation_policy,
            interaction_event,
            activity_rx,
            pending_activity,
        } = request;

        let locked_process = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            Arc::clone(&entry.process)
        };
        let _interaction_guard = locked_process.interaction_lock().lock_owned().await;

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
            .prepare_process_handles(process_id, &locked_process)
            .await?;
        let start = Instant::now();
        let mut state_rx = process.subscribe_state();
        let has_buffered_output = tty && output.output_buffer.lock().await.total_bytes() > 0;

        let wait_reason = if process.has_exited() || process.failure_message().is_some() {
            ProcessWaitReason::Completed
        } else if pending_activity.is_some() {
            ProcessWaitReason::Input
        } else if has_buffered_output {
            ProcessWaitReason::Output
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

            loop {
                // Register all event listeners before re-checking state so no
                // process, output, or input event can be lost in between.
                let state_changed = state_rx.changed();
                let activity_changed = activity_rx.changed();
                let output_notified = output.output_notify.notified();
                tokio::pin!(state_changed);
                tokio::pin!(activity_changed);
                tokio::pin!(output_notified);

                if process.has_exited() || process.failure_message().is_some() {
                    break ProcessWaitReason::Completed;
                }
                if tty && output.output_buffer.lock().await.total_bytes() > 0 {
                    break ProcessWaitReason::Output;
                }

                tokio::select! {
                    biased;
                    _ = &mut state_changed => {}
                    _ = &mut activity_changed => break ProcessWaitReason::Input,
                    _ = &mut output_notified, if tty => {}
                }
            }
        };

        // Event-driven waiting ends above. A completed process gets only the
        // existing bounded grace period to flush output already in flight.
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
            self.release_process_id(process_id).await;
            return Err(fail_process_with_message(process.as_ref(), message));
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                session.as_ref(),
                network_approval.clone(),
            )
            .await;
            self.release_process_id(process_id).await;
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }

        let status = self.refresh_process_state(process_id).await;
        let (process_id, exit_code, event_call_id, wait_reason) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id, wait_reason),
            ProcessStatus::Exited { exit_code, entry } => {
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
