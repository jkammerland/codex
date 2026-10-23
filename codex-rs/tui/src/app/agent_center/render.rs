//! Status tabs, task rows, the rename editor and details share a wide/narrow layout.

use super::hints::hint_line;
use super::*;
use crate::bottom_pane::render_filled_tab_bar;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crossterm::cursor::SetCursorStyle;
use ratatui::widgets::WidgetRef;
use unicode_segmentation::UnicodeSegmentation;

pub(super) fn row(area: Rect, offset: u16, height: u16) -> Rect {
    let offset = offset.min(area.height);
    Rect::new(
        area.x,
        area.y + offset,
        area.width,
        height.min(area.height - offset),
    )
}

pub(super) fn line(text: impl Into<Line<'static>>, area: Rect, buf: &mut Buffer) {
    if !area.is_empty() {
        truncate_line_with_ellipsis_if_overflow(text.into(), usize::from(area.width))
            .render(row(area, /*offset*/ 0, /*height*/ 1), buf);
    }
}

fn visible_suffix(input: &str, available: usize) -> &str {
    let mut start = input.len();
    let mut visible_width = 0;
    for (offset, grapheme) in input.grapheme_indices(/*is_extended*/ true).rev() {
        let width = grapheme.width();
        if visible_width + width > available {
            break;
        }
        visible_width += width;
        start = offset;
    }
    &input[start..]
}

fn center_notice(state: &AgentsOverviewViewState) -> Option<String> {
    if state.help || state.key_chord_hint.is_some() {
        return None;
    }
    state
        .connection_notice
        .map(str::to_owned)
        .or_else(|| state.team_notice.clone())
        .or_else(|| {
            state
                .creating_worktree
                .then(|| "Creating worktree…".to_owned())
        })
        .or_else(|| {
            state.removal.map(|action| match action {
                AgentsOverviewAction::Archive => {
                    "Archiving task…  You can still browse.".to_owned()
                }
                AgentsOverviewAction::Delete => "Deleting task…  You can still browse.".to_owned(),
            })
        })
        .or_else(|| {
            (state.refresh_failed && !state.loading).then(|| "Error loading tasks".to_owned())
        })
        .or_else(|| state.server_version_notice.clone())
}

struct CenterLayout {
    header: Rect,
    hosts: Rect,
    host_gap: Rect,
    list: Rect,
    gap: Rect,
    details: Rect,
    search: Rect,
    notice: Rect,
    footer: Rect,
}

impl AgentsOverviewView {
    fn center_layout(&self, area: Rect) -> CenterLayout {
        let mut state = self.state();
        // Persistent team status must not replace navigation and quit hints.
        let footer_height = u16::from(area.height >= 2);
        let notice_height = u16::from(area.height >= 6 && center_notice(&state).is_some());
        let header_height = if area.height >= 6 { 3 } else { 0 };
        let footer = row(area, area.height - footer_height, footer_height);
        let notice = row(
            area,
            area.height - footer_height - notice_height,
            notice_height,
        );
        let header = row(area, /*offset*/ 0, header_height);
        let body = row(area, header_height, notice.y - header.bottom())
            .inner(Margin::new(/*horizontal*/ 2, /*vertical*/ 0));
        state.hosts_visible = area.width >= 128 && !body.is_empty() && !state.hosts.is_empty();
        if !state.hosts_visible {
            state.hosts_focused = false;
        }
        let (hosts, host_gap, body) = if state.hosts_visible {
            let [hosts, gap, body] = Layout::horizontal([
                Constraint::Length(26),
                Constraint::Length(3),
                Constraint::Min(0),
            ])
            .areas(body);
            (hosts, gap, body)
        } else {
            (Rect::default(), Rect::default(), body)
        };
        let [list, gap, details] = if body.width >= 90 {
            // Give details room to breathe on wide terminals while keeping the task list usable.
            let details_width = (body.width / 5 * 2).clamp(38, 64);
            Layout::horizontal([
                Constraint::Min(46),
                Constraint::Length(3),
                Constraint::Length(details_width),
            ])
            .areas(body)
        } else {
            [body, Rect::default(), Rect::default()]
        };
        let search = row(list, /*offset*/ 0, u16::from(state.editing_metadata()));
        let list = row(
            list,
            search.height,
            list.height.saturating_sub(search.height),
        );
        CenterLayout {
            header,
            hosts,
            host_gap,
            list,
            gap,
            details,
            search,
            notice,
            footer,
        }
    }
}

impl Renderable for AgentsOverviewView {
    fn desired_height(&self, _width: u16) -> u16 {
        24
    }

    fn cursor_style(&self, _area: Rect) -> SetCursorStyle {
        let state = self.state();
        if state.rename_target.is_some() && state.input.uses_vim_insert_cursor() {
            SetCursorStyle::SteadyBar
        } else {
            SetCursorStyle::DefaultUserShape
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let layout = self.center_layout(area);
        let state = self.state();
        if state.editing_metadata() && !layout.search.is_empty() {
            if state.rename_target.is_some() {
                let mut area = layout.search;
                let prefix_width = 9.min(area.width);
                area.x += prefix_width;
                area.width -= prefix_width;
                return state.input.cursor_pos(area);
            }
            let input = &state.search;
            let prefix = "Search › ";
            let available = usize::from(layout.search.width).saturating_sub(prefix.width() + 1);
            return Some((
                layout.search.x
                    + (prefix.width() + visible_suffix(input, available).width())
                        .min(usize::from(layout.search.width.saturating_sub(/*rhs*/ 1)))
                        as u16,
                layout.search.y,
            ));
        }
        None
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let layout = self.center_layout(area);
        let inset =
            |area: Rect| area.inner(Margin::new(/*horizontal*/ 2, /*vertical*/ 0));
        let footer_hints = self.center_footer_hints();
        let state = self.state();
        let filter_keys = if state.editing_metadata() {
            String::new()
        } else {
            self.center_filter_hint()
        };
        let grouping = match state.grouping {
            AgentsOverviewGrouping::Project => "Project",
            AgentsOverviewGrouping::Directory => "Directory",
            AgentsOverviewGrouping::Status => "Status",
            AgentsOverviewGrouping::Model => "Model",
        };
        let group_key = self
            .agents_keymap
            .primary_hint("toggle_grouping", &self.agents_keymap.toggle_grouping)
            .map(ShortcutHint::display_label)
            .unwrap_or_default();
        // Keep section navigation visible before optional grouping details on narrow terminals.
        let mut header = vec![
            if area.width < 64 {
                "Agents"
            } else {
                "Agent command center"
            }
            .bold(),
            "   t Tasks   b Board".cyan(),
        ];
        if area.width >= 64 {
            header.push(format!("  Group: {grouping}  {group_key}").dim());
        }
        line(header, inset(layout.header), buf);
        let labels = TASK_FILTERS
            .iter()
            .map(|(label, group)| {
                let count = self
                    .rows
                    .iter()
                    .filter(|row| {
                        state
                            .host_filter
                            .as_ref()
                            .is_none_or(|host| row.host.as_ref() == Some(host))
                            && group.is_none_or(|group| group == row.group)
                    })
                    .count();
                format!("{label} {count}")
            })
            .collect::<Vec<_>>();
        let mut tabs = inset(row(layout.header, /*offset*/ 1, /*height*/ 1));
        if state.filters_focused && tabs.width > 2 {
            line("› ".set_style(crate::style::accent_style()), tabs, buf);
            tabs.x += 2;
            tabs.width -= 2;
        }
        let filter_hint = hint_line(&[(filter_keys, "focus".into())]);
        if tabs.width >= 80 && filter_hint.width() > 0 {
            let hint_width = filter_hint.width() as u16;
            filter_hint.render(
                Rect::new(tabs.right() - hint_width, tabs.y, hint_width, tabs.height),
                buf,
            );
            tabs.width = tabs.width.saturating_sub(hint_width + 2);
        }
        render_filled_tab_bar(
            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            state.status_filter,
            tabs,
            buf,
        );
        let scope = if !state.hosts.is_empty() && layout.hosts.is_empty() {
            let host = state
                .host_filter
                .as_ref()
                .and_then(|key| state.hosts.iter().find(|host| &host.key == key));
            host.map_or_else(
                || format!("Hosts: All {} · F9 machines", state.hosts.len()),
                |host| {
                    format!(
                        "Host: {} · {}",
                        host.label,
                        if host.connected {
                            "Connected"
                        } else {
                            "Offline"
                        }
                    )
                },
            )
        } else {
            "─".repeat(usize::from(area.width.saturating_sub(4)))
        };
        line(scope.dim(), inset(row(layout.header, 2, 1)), buf);
        if state.editing_metadata() {
            buf.set_style(layout.search, crate::bottom_pane::active_tab_style());
            if state.rename_target.is_some() {
                line("Rename › ".cyan().bold(), layout.search, buf);
                let mut area = layout.search;
                let prefix_width = 9.min(area.width);
                area.x += prefix_width;
                area.width -= prefix_width;
                WidgetRef::render_ref(&&state.input, area, buf);
            } else {
                let label = "Search › ";
                let available = usize::from(layout.search.width).saturating_sub(label.width() + 1);
                line(
                    vec![
                        label.cyan().bold(),
                        visible_suffix(&state.search, available).to_owned().into(),
                    ],
                    layout.search,
                    buf,
                );
            }
        }
        if let Some(notice) = center_notice(&state) {
            line(notice.dim(), inset(layout.notice), buf);
        }
        line(hint_line(&footer_hints), inset(layout.footer), buf);
        let help = state.help;
        if !help && !layout.hosts.is_empty() {
            let counts = state
                .hosts
                .iter()
                .map(|host| {
                    self.rows
                        .iter()
                        .filter(|row| row.host.as_ref() == Some(&host.key))
                        .count()
                })
                .collect::<Vec<_>>();
            super::hosts::render_hosts(
                &state.hosts,
                &counts,
                super::hosts::HostCount::Tasks,
                state.host_filter.as_ref(),
                state.hosts_focused,
                layout.hosts,
                buf,
            );
            super::hosts::divider(layout.host_gap, buf);
        }
        drop(state);
        if help {
            let body = row(
                area,
                layout.header.height,
                layout.footer.y - layout.header.bottom(),
            );
            Clear.render(inset(body), buf);
            let body = inset(body);
            let mut lines = self.center_help_lines(body.width);
            if lines.len() > usize::from(body.height) {
                lines.truncate(usize::from(body.height.saturating_sub(/*rhs*/ 1)));
                lines.push("… resize to see all".dim().into());
            }
            Paragraph::new(lines).render(body, buf);
            return;
        }
        super::hosts::divider(layout.gap, buf);
        self.render_center_rows(layout.list, buf);
        if !layout.details.is_empty() {
            self.render_details(layout.details, buf);
        }
    }
}
