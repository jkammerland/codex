//! Keep passive-footer controls scoped to the visible composer, never a modal backdrop.

use super::*;

impl BottomPane {
    pub(crate) fn set_rewind_available(&mut self, available: bool) {
        self.composer.set_rewind_available(available);
    }

    pub(crate) fn set_agent_navigation_available(&mut self, available: bool) {
        self.composer.set_agent_navigation_available(available);
    }

    pub(crate) fn clear_footer_hit_targets(&self) {
        self.composer.clear_footer_hit_targets();
    }

    pub(crate) fn rewind_contains(&self, position: ratatui::layout::Position) -> bool {
        self.no_modal_or_popup_active() && self.composer.rewind_contains(position)
    }
}
