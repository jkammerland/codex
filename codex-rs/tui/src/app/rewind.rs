//! Explicit prompt selection shares eligibility with confirmation and the passive footer.

use super::*;

impl App {
    pub(crate) fn rewind_unavailable_reason(&self) -> Option<&'static str> {
        let Some(thread_id) = self.chat_widget.thread_id() else {
            return Some("Start a conversation before rewinding.");
        };
        if self.is_offline() || self.thread_unavailable(thread_id) {
            return Some("Reconnect to this conversation before rewinding.");
        }
        if self.chat_widget.is_external_writer_view() {
            return Some("Rewind is unavailable in a read-only conversation.");
        }
        if self.chat_widget.side_conversation_active() {
            return Some(crate::app_backtrack::SIDE_EDIT_PREVIOUS_UNAVAILABLE_MESSAGE);
        }
        if self.chat_widget.is_user_turn_pending_or_running() {
            return Some("Wait for the current turn to finish before rewinding.");
        }
        if self.pending_host_prompt_reverts.contains_key(&thread_id)
            || self.pending_server_profiles.contains_key(&thread_id)
            || self.chat_widget.fork_in_progress
        {
            return Some("Wait for the current conversation change to finish before rewinding.");
        }
        if !self.chat_widget.rewind_draft_is_empty() {
            return Some("Clear the current draft and attachments before rewinding.");
        }
        if !self.chat_widget.is_normal_backtrack_mode()
            || self
                .overlay
                .as_ref()
                .is_some_and(|overlay| !matches!(overlay, Overlay::Transcript(_)))
        {
            return Some("Close the current interaction before rewinding.");
        }
        None
    }

    pub(super) fn refresh_rewind_control(&mut self) {
        self.chat_widget
            .set_rewind_available(self.rewind_unavailable_reason().is_none());
        self.chat_widget.set_agent_navigation_available(
            self.agent_center_session_navigation() && !self.backtrack.overlay_preview_active,
        );
    }
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
