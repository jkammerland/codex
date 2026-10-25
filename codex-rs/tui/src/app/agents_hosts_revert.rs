//! Prompt editing completes through the owning host's sole notification intake.

use super::*;

pub(super) struct PendingHostPromptRevert {
    pub(super) prompt: crate::chatwidget::UserMessage,
    pub(super) retained_turns: Vec<Turn>,
    pub(super) reverted: Option<codex_app_server_protocol::ThreadRevertResponse>,
    pub(super) request_id: Uuid,
    pub(super) error: Option<super::agents_hosts_revert_preparation::PromptRevertFailure>,
    pub(super) selected_cell: Arc<dyn HistoryCell>,
    pub(super) notified: bool,
}

impl App {
    pub(super) async fn complete_host_prompt_revert(&mut self, thread_id: ThreadId) -> Result<()> {
        if self.current_displayed_thread_id() != Some(thread_id)
            || !self
                .pending_host_prompt_reverts
                .get(&thread_id)
                .is_some_and(|pending| {
                    pending.error.is_some() || (pending.notified && pending.reverted.is_some())
                })
        {
            return Ok(());
        }
        #[expect(
            clippy::expect_used,
            reason = "The guard checked this pending entry and no mutation or await intervenes."
        )]
        let mut pending = self
            .pending_host_prompt_reverts
            .remove(&thread_id)
            .expect("pending revert was checked");
        if let Some(error) = pending.error {
            self.chat_widget
                .restore_user_message_to_composer(pending.prompt);
            self.chat_widget
                .set_queue_autosend_suppressed(/*suppressed*/ false);
            if error.reload_required {
                self.chat_widget.show_external_writer_thread();
            }
            self.chat_widget.add_error_message(error.message);
            return Ok(());
        }
        #[expect(
            clippy::expect_used,
            reason = "The guard requires a response unless an error exists, which returned above."
        )]
        let reverted = pending
            .reverted
            .expect("notified revert has its authoritative response");
        // Preserve the widget and unrelated threads; only replace this replay store.
        self.chat_widget.restore_thread_input_state(
            /*input_state*/ None,
            crate::chatwidget::ThreadInputStateRestoreMode {
                preserve_in_flight_turn: false,
            },
        );
        self.chat_widget
            .restore_user_message_to_composer(pending.prompt);
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        self.abort_thread_event_listener(thread_id);
        if let Some(mut rx) = self.active_thread_rx.take() {
            while let Ok(event) = rx.try_recv() {
                if let ThreadBufferedEvent::Notification(notification) = event {
                    self.chat_widget.handle_server_notification(
                        *notification,
                        Some(ReplayKind::ThreadSnapshot),
                    );
                }
            }
        }
        let mut session = {
            let store = self
                .thread_event_channels
                .get(&thread_id)
                .ok_or_else(|| color_eyre::eyre::eyre!("reverted thread is no longer available"))?
                .store
                .lock()
                .await;

            // Older pages may complete after preparation captured the visible window.
            // Merge only the retained prefix, never the selected turn or later events.
            if let Some(last_retained) = store.turns.iter().rposition(|turn| {
                pending
                    .retained_turns
                    .iter()
                    .any(|retained| retained.id == turn.id)
            }) {
                super::history_pagination::merge_older_turns(
                    &mut pending.retained_turns,
                    store.turns[..=last_retained].to_vec(),
                );
            }

            store
                .session
                .clone()
                .ok_or_else(|| color_eyre::eyre::eyre!("reverted thread has no session"))?
        };
        session.rollout_path = reverted.thread.path.clone();
        if self.primary_thread_id == Some(thread_id) {
            self.primary_session_configured = Some(session.clone());
        }
        self.thread_event_channels.remove(&thread_id);
        self.active_thread_id = None;
        self.recap.reset_for_new_thread(Instant::now());
        self.recap
            .seed_from_turns(&pending.retained_turns, Instant::now());
        self.retain_realtime_replay_state_before_replace();
        self.forget_realtime_replay_thread(thread_id);
        self.chat_widget
            .reset_after_prompt_revert(reverted.thread.path, &pending.retained_turns);
        self.ensure_thread_channel(thread_id)
            .store
            .lock()
            .await
            .set_session(session, pending.retained_turns);
        self.activate_thread_channel(thread_id).await;
        // Apply the trim after any transcript inserts produced by shutdown notifications.
        // The existing reset barrier keeps terminal input blocked until then.
        self.pending_thread_switch_resets += 1;
        self.app_event_tx.send(AppEvent::FinishPromptRevert {
            thread_id,
            selected_cell: pending.selected_cell,
        });
        Ok(())
    }
}
