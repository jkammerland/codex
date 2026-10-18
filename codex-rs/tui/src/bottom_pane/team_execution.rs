//! Persistent team admission status stays visible while the composer remains editable.

use super::*;
use crate::chatwidget::team_execution::TeamExecution;

pub(super) struct TeamExecutionStatus(pub(super) &'static str);

impl Renderable for TeamExecutionStatus {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        let lines = crate::wrapping::word_wrap_lines(
            &[Line::from(self.0).style(crate::style::status_style(
                crate::style::StatusTone::Attention,
            ))],
            usize::from(area.width),
        );
        Renderable::render(&Paragraph::new(lines), area, buffer);
    }

    fn desired_height(&self, width: u16) -> u16 {
        crate::wrapping::word_wrap_lines(&[Line::from(self.0)], usize::from(width)).len() as u16
    }
}

impl BottomPane {
    pub(crate) fn set_team_execution(&mut self, execution: Option<TeamExecution>) {
        self.team_execution_status = execution.and_then(TeamExecution::status);
        self.composer
            .set_team_execution_held(execution.is_some_and(TeamExecution::held));
        self.request_redraw();
    }
}
