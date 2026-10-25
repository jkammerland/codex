//! Enter prompt selection directly, without borrowing the Escape navigation gesture.

use super::*;

impl App {
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
