//! Enter prompt selection directly, without borrowing the Escape navigation gesture.

use super::*;

impl App {
    pub(crate) fn open_rewind_from_composer(
        &mut self,
        tui: &mut tui::Tui,
        thread_id: Option<ThreadId>,
        draft: &crate::bottom_pane::ComposerDraftSnapshot,
    ) {
        // The queued request owns only this thread's unchanged command draft.
        if thread_id != self.chat_widget.thread_id()
            || !self.chat_widget.rewind_command_matches(draft)
        {
            return;
        }
        let unavailable = self.rewind_conversation_unavailable_reason().or_else(|| {
            (!has_backtrack_target(&self.transcript_cells) && !self.scrollback_has_older_history)
                .then_some(NO_PREVIOUS_MESSAGE_TO_EDIT)
        });
        if let Some(reason) = unavailable {
            self.chat_widget
                .add_info_message(reason.to_string(), /*hint*/ None);
            tui.frame_requester().schedule_frame();
            return;
        }

        self.chat_widget.accept_rewind_command();
        self.open_rewind(tui);
    }

    pub(crate) fn open_rewind(&mut self, tui: &mut tui::Tui) {
        if let Some(reason) = self.rewind_unavailable_reason() {
            self.chat_widget
                .add_info_message(reason.to_string(), /*hint*/ None);
        } else if !self.backtrack.overlay_preview_active {
            self.remember_browsing_origin(tui);
            if matches!(self.overlay, Some(Overlay::Transcript(_))) {
                self.begin_overlay_backtrack_preview(tui);
            } else {
                self.prime_backtrack();
                self.open_backtrack_preview(tui);
            }
            self.backtrack.explicit_rewind = self.backtrack.overlay_preview_active;
        }
        tui.frame_requester().schedule_frame();
    }
}
