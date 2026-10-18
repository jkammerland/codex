//! Team admission is independent of turn activity, editing, and queue delivery state.
//! Only an accepted coordinator snapshot releases the gate after a held rejection.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TeamExecution {
    Running,
    Paused,
    Stopped,
    Resuming,
    Syncing,
}

impl TeamExecution {
    pub(crate) fn held(self) -> bool {
        self != Self::Running
    }

    pub(crate) fn status(self) -> Option<&'static str> {
        match self {
            Self::Running => None,
            Self::Paused => Some("Team paused · F7 resume team · /agents controls"),
            Self::Stopped => Some("Team stopped · F7 resume team · /agents controls"),
            Self::Resuming => Some("Team resuming · /agents controls"),
            Self::Syncing => Some("Team syncing · /agents controls"),
        }
    }
}

impl ChatWidget {
    pub(crate) fn team_execution_held(&self) -> bool {
        self.team_execution.is_some_and(TeamExecution::held)
    }

    pub(crate) fn set_team_execution(&mut self, execution: Option<TeamExecution>) {
        let was_held = self.team_execution_held();
        self.team_execution = execution;
        self.bottom_pane.set_team_execution(execution);
        self.request_redraw();
        if was_held && !self.team_execution_held() {
            self.maybe_send_next_queued_input();
        }
    }

    /// Recover exactly the rejected request; a steer rejection does not end its active turn.
    pub(crate) fn restore_team_held_submission(&mut self, client_id: &str) -> bool {
        let message = if self.input_queue.pending_user_message_client_id.as_deref()
            == Some(client_id)
            && self.input_queue.user_turn_pending_start
        {
            // Finalization clears safety buffering, so take the rich prompt first.
            let Some(message) = self.safety_buffering_prompt.take() else {
                return false;
            };
            self.finalize_turn();
            self.input_queue.pending_user_message_client_id = None;
            message
        } else if let Some(index) = self
            .input_queue
            .pending_steers
            .iter()
            .position(|pending| pending.client_id == client_id)
        {
            let Some(pending) = self.input_queue.pending_steers.remove(index) else {
                return false;
            };
            user_message_for_restore(pending.user_message, &pending.history_record)
        } else {
            return false;
        };

        // Do not change queue delivery flags or enqueue the restored draft. A later Running
        // snapshot may drain pre-existing work, but this text still requires an explicit submit.
        if !self.team_execution_held() {
            self.set_team_execution(Some(TeamExecution::Syncing));
        }
        self.restore_user_message_to_composer(message);
        self.refresh_pending_input_preview();
        self.add_info_message("Not sent; draft restored".into(), None);
        self.request_redraw();
        true
    }
}

impl ThreadInputState {
    /// Retained tasks recover into their own draft, preserving any newer editing state.
    pub(crate) fn restore_team_held_submission(&mut self, client_id: &str) -> bool {
        let message = if self.user_turn_pending_start
            && self.pending_user_message_client_id.as_deref() == Some(client_id)
        {
            let Some(message) = self.safety_buffering_prompt.take() else {
                return false;
            };
            self.user_turn_pending_start = false;
            self.pending_user_message_client_id = None;
            self.task_running = self.agent_turn_running;
            message
        } else if let Some(index) = self
            .pending_steers
            .iter()
            .position(|pending| pending.client_id == client_id)
        {
            let Some(pending) = self.pending_steers.remove(index) else {
                return false;
            };
            user_message_for_restore(pending.user_message, &pending.history_record)
        } else {
            return false;
        };
        let mut composer = self.composer.take().unwrap_or_default();
        let message = if composer.has_content() {
            merge_user_messages(vec![
                message,
                UserMessage {
                    text: composer.text,
                    local_images: composer.local_images,
                    remote_image_urls: composer.remote_image_urls,
                    text_elements: composer.text_elements,
                    mention_bindings: composer.mention_bindings,
                },
            ])
        } else {
            message
        };
        composer.text = message.text;
        composer.local_images = message.local_images;
        composer.remote_image_urls = message.remote_image_urls;
        composer.text_elements = message.text_elements;
        composer.mention_bindings = message.mention_bindings;
        self.composer = Some(composer);
        true
    }
}
