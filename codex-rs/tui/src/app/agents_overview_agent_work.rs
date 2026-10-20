//! Mail delivery and barrier progress remain separate from execution status.

use codex_app_server_protocol::ThreadAgentWork;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::style::StatusTone;
use crate::style::accent_style;
use crate::style::secondary_text_style;
use crate::style::status_style;

pub(super) fn detail_lines(work: Option<&ThreadAgentWork>) -> Vec<Line<'static>> {
    let Some(work) = work else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("Mail      ", secondary_text_style()),
        Span::styled(
            format!("{} queued", work.pending_mail),
            if work.pending_mail == 0 {
                secondary_text_style()
            } else {
                accent_style()
            },
        ),
    ])];
    if let Some(waiting) = &work.waiting_tasks {
        lines.push(Line::from(vec![
            Span::styled("Barrier   ", secondary_text_style()),
            Span::styled(
                format!(
                    "Waiting for {} of {} tasks",
                    waiting.remaining, waiting.total
                ),
                accent_style(),
            ),
        ]));
        if waiting.held != 0 || waiting.unavailable != 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "          {} held · {} unavailable",
                    waiting.held, waiting.unavailable
                ),
                status_style(StatusTone::Attention),
            )));
        }
    }
    lines
}

#[cfg(test)]
#[path = "agents_overview_agent_work_tests.rs"]
mod tests;
