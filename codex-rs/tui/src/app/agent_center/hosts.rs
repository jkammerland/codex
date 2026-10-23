//! Shared host presence sidebar. Board presence never filters its cross-host conversations.

use super::render::line;
use super::render::row;
use crate::app::agents_hosts::HostKey;
use crate::bottom_pane::selection_style;
use crate::style::StatusTone;
use crate::style::status_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::text::Line;

#[derive(Clone)]
pub(crate) struct HostSummary {
    pub(crate) key: HostKey,
    pub(crate) label: String,
    pub(crate) connected: bool,
}

pub(super) enum HostCount {
    Tasks,
    Agents,
}

pub(super) fn divider(area: Rect, buffer: &mut Buffer) {
    for y in area.y..area.bottom() {
        if !area.is_empty() {
            buffer[(area.x + area.width / 2, y)]
                .set_symbol("│")
                .set_style(Style::default().dim());
        }
    }
}

pub(super) fn render_hosts(
    hosts: &[HostSummary],
    counts: &[usize],
    count_kind: HostCount,
    selected: Option<&HostKey>,
    focused: bool,
    area: Rect,
    buffer: &mut Buffer,
) {
    let count_label = match count_kind {
        HostCount::Tasks => "tasks",
        HostCount::Agents => "agents",
    };
    line(
        if focused { "› Hosts" } else { "Hosts" }.bold(),
        area,
        buffer,
    );
    let all_style = if selected.is_none() && focused {
        selection_style()
    } else {
        Style::default()
    };
    line(
        format!(
            "{} All hosts · {}",
            if selected.is_none() { "›" } else { " " },
            hosts.len()
        )
        .set_style(all_style),
        row(area, 1, 1),
        buffer,
    );

    // Keep the selected host visible without storing a second, drifting scroll position.
    let capacity = usize::from(area.height.saturating_sub(2) / 3).max(1);
    let selected_index = hosts.iter().position(|host| Some(&host.key) == selected);
    let start = selected_index.map_or(0, |index| index.saturating_add(1).saturating_sub(capacity));
    for (index, host) in hosts.iter().enumerate().skip(start).take(capacity) {
        let offset = 2 + (index - start) as u16 * 3;
        let selected = selected == Some(&host.key);
        let style = if selected && focused {
            selection_style()
        } else {
            Style::default()
        };
        let label = format!("{} {}", if selected { "›" } else { " " }, host.label);
        line(label.set_style(style).bold(), row(area, offset, 1), buffer);
        line(
            format!(
                "  {} · {} {count_label}",
                if host.key == HostKey::default() {
                    "App server"
                } else {
                    host.key.server_id.as_str()
                },
                counts.get(index).copied().unwrap_or_default()
            )
            .dim(),
            row(area, offset + 1, 1),
            buffer,
        );
        line(
            Line::from(vec![
                "  ".into(),
                if host.connected {
                    "● Connected".set_style(status_style(StatusTone::Success))
                } else {
                    "○ Offline".dim()
                },
            ]),
            row(area, offset + 2, 1),
            buffer,
        );
    }
}
