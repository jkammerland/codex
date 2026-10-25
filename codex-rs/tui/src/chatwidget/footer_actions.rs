//! Activate passive footer controls only after the selection owner releases a stationary click.

use super::*;
use crate::tui::TuiEvent;

impl ChatWidget {
    pub(crate) fn rewind_draft_is_empty(&self) -> bool {
        self.composer_is_empty() && self.bottom_pane.composer_pending_pastes().is_empty()
    }

    pub(crate) fn set_rewind_available(&mut self, available: bool) {
        self.bottom_pane.set_rewind_available(available);
    }

    pub(crate) fn set_agent_navigation_available(&mut self, available: bool) {
        self.bottom_pane.set_agent_navigation_available(available);
    }

    pub(crate) fn clear_footer_hit_targets(&self) {
        self.bottom_pane.clear_footer_hit_targets();
    }

    pub(crate) fn handle_footer_event(
        &mut self,
        event: &TuiEvent,
        cells: &[Arc<dyn HistoryCell>],
    ) -> bool {
        if self.handle_warning_event(event, cells) {
            return true;
        }
        if let TuiEvent::Mouse(mouse) = event
            && mouse.kind
                == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
            && self
                .bottom_pane
                .rewind_contains(ratatui::layout::Position::new(mouse.column, mouse.row))
        {
            self.app_event_tx.send(AppEvent::OpenRewind);
            return true;
        }
        false
    }
}

#[cfg(test)]
#[path = "footer_actions_tests.rs"]
mod tests;
