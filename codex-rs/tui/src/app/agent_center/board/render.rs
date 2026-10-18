use super::*;
use crate::bottom_pane::render_filled_tab_bar;
use crate::render::line_utils::line_to_static;
use crate::render::renderable::Renderable;
use crate::wrapping::word_wrap_line_with_source;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

fn row(area: Rect, offset: u16, height: u16) -> Rect {
    let offset = offset.min(area.height);
    Rect::new(
        area.x,
        area.y + offset,
        area.width,
        height.min(area.height - offset),
    )
}

pub(super) fn author(entry: &TeamBoardEntry) -> String {
    if entry.author.identity == TeamBoardIdentity::User {
        return "You · Human".into();
    }
    format!(
        "{} · {}{}",
        entry.author.name,
        entry.author.machine_name.as_deref().unwrap_or("Agent"),
        if entry.author.legacy {
            " · historical identity"
        } else {
            ""
        }
    )
}

pub(super) fn time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |time| time.format("%b %d %H:%M").to_string(),
    )
}

fn key_hints(items: &[(&str, &str)]) -> Line<'static> {
    let mut line = Line::default();
    for (key, label) in items {
        if !line.spans.is_empty() {
            line.spans
                .push(" · ".set_style(crate::style::secondary_text_style()));
        }
        line.spans.extend(crate::key_hint::key_label_spans(key));
        if !label.is_empty() {
            line.spans
                .push(format!(" {label}").set_style(crate::style::secondary_text_style()));
        }
    }
    line
}

impl BoardView {
    fn composer_area(&self, state: &BoardState, area: Rect) -> Rect {
        let height = if state.composing.is_some() {
            state
                .composer
                .as_ref()
                .map_or(0, |composer| composer.desired_height(area.width))
                .min(area.height.saturating_sub(4))
        } else {
            0
        };
        row(area, area.height.saturating_sub(height + 1), height)
    }
}

impl Renderable for BoardView {
    fn desired_height(&self, _width: u16) -> u16 {
        24
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let state = self.state();
        if state.composing.is_some() {
            return state
                .composer
                .as_ref()
                .and_then(|composer| composer.cursor_pos(self.composer_area(&state, area)));
        }
        None
    }
    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        let state = self.state();
        state.composer.as_ref().map_or(
            crossterm::cursor::SetCursorStyle::DefaultUserShape,
            |composer| composer.cursor_style(self.composer_area(&state, area)),
        )
    }
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        Clear.render(area, buffer);
        if area.is_empty() {
            return;
        }
        let mut state = self.state();
        state.displayed = None;
        let compact = state.team_control.is_some() && area.width < 80;
        let mut title = vec![
            if compact {
                "Agents"
            } else {
                "Agent command center"
            }
            .bold(),
            if compact {
                "  t Tasks  "
            } else {
                "   t Tasks   "
            }
            .dim(),
            "b Board".cyan().bold(),
        ];
        if let Some(control) = state.team_control {
            let summary = if compact {
                format!("  {:?} {}/{}", control.mode, control.applied, control.total)
            } else {
                format!("  {control}")
            };
            title.push(summary.set_style(crate::style::secondary_text_style()));
        }
        let mut title = Line::from(title);
        if compact && title.width() > usize::from(area.width) {
            title.spans.remove(0);
            if let Some(tasks) = title.spans.first_mut() {
                *tasks = "t Tasks  ".dim();
            }
        }
        Paragraph::new(title).render(row(area, 0, 1), buffer);
        if area.height < 4 {
            return;
        }
        if state.topics_open {
            Paragraph::new("Topics · conversations and recent activity".bold())
                .render(row(area, 1, 1), buffer);
        } else {
            render_filled_tab_bar(
                &["All posts", "Following", "Mentions"],
                match state.filter {
                    Filter::All => 0,
                    Filter::Following => 1,
                    Filter::Mentions => 2,
                },
                row(area, 1, 1),
                buffer,
            );
        }
        let subtitle: Line = if state.editing_topic {
            format!("New post topic › #{}", state.topic).dim().into()
        } else if state.searching {
            format!("Search › {}", state.query).dim().into()
        } else if state.composing.is_some() && !state.query.is_empty() {
            format!("Search: {}  · / edit · #topic filters topics", state.query)
                .dim()
                .into()
        } else if state.composing.is_some() {
            "Posts and conversations · / search · #topic in search · # set post topic"
                .dim()
                .into()
        } else if state.topics_open {
            if state.topic_preview_focused {
                key_hints(&[
                    ("j/k PgUp/PgDn", "scroll"),
                    ("Home/End", ""),
                    ("Tab/h", "topics"),
                ])
            } else {
                key_hints(&[
                    ("j/k", "move"),
                    ("l/Enter", "open"),
                    ("Tab", "preview"),
                    ("h/Esc", "posts"),
                    ("/", "search"),
                ])
            }
        } else if state.followers {
            key_hints(&[("v/h/←/Esc", "close"), ("F", "manage agents")])
        } else if state.opened.is_some() {
            key_hints(&[("j/k ↑/↓", "scroll"), ("PgUp/PgDn", ""), ("Home/End", "")])
        } else if let Some(topic) = &state.topic_filter {
            format!("#{topic} · h back to topics · l/Enter read · j/k move")
                .dim()
                .into()
        } else if area.width < 80 {
            key_hints(&[("j/k ↑/↓", "move"), ("l/→/Enter", "open")])
        } else {
            let search = if state.query.is_empty() {
                "search".into()
            } else {
                format!("search: {}", state.query)
            };
            key_hints(&[
                ("j/k ↑/↓", "move"),
                ("l/→/Enter", "open"),
                ("Tab", "filter"),
                ("/", &search),
                ("#", "set post topic"),
                ("o", "topics"),
            ])
        };
        Paragraph::new(subtitle).render(row(area, 2, 1), buffer);
        let composer_area = self.composer_area(&state, area);
        let body_end = if composer_area.height > 0 {
            composer_area.y
        } else {
            area.bottom().saturating_sub(1)
        };
        let body = row(area, 3, body_end.saturating_sub(area.y + 3));
        if state.topics_open {
            super::topics::render_topics(&mut state, body, buffer);
        } else {
            let wide = body.width >= 100;
            let (feed, conversation) = if wide {
                let [feed, _, conversation] = Layout::horizontal([
                    Constraint::Percentage(40),
                    Constraint::Length(2),
                    Constraint::Min(0),
                ])
                .areas(body);
                (feed, conversation)
            } else if state.opened.is_some() || state.followers {
                (Rect::default(), body)
            } else {
                (body, Rect::default())
            };
            state.page_height = usize::from(conversation.height.saturating_sub(2)).max(1);
            if !feed.is_empty() {
                render_feed(&mut state, feed, buffer);
            }
            if !conversation.is_empty() {
                render_conversation(&mut state, conversation, buffer);
            }
        }
        if let Some(composer) = state
            .composer
            .as_ref()
            .filter(|_| !composer_area.is_empty())
        {
            composer.render(composer_area, buffer);
        }
        let footer: Line = if state.composing.is_some() {
            state
                .notice
                .clone()
                .unwrap_or_else(|| {
                    "@ mention · Enter send · Esc keep draft · F6 pause · F7 resume · F8 stop"
                        .into()
                })
                .dim()
                .into()
        } else if state.topics_open {
            state.notice.as_ref().map_or_else(
                || {
                    key_hints(&[
                        ("f", "watch/unwatch"),
                        ("F", "agent subscriptions"),
                        ("n", "post"),
                        ("t", "Tasks"),
                    ])
                },
                |notice| Line::from(notice.clone().dim()),
            )
        } else if state.followers {
            key_hints(&[("v/Esc", "close followers"), ("t", "Tasks")])
        } else if area.width < 80 && state.opened.is_none() && !state.query.is_empty() {
            format!("Search: {} · / edit", state.query).dim().into()
        } else if let Some(notice) = &state.notice {
            format!("{notice} · n post · r reply · f follow · v followers · t Tasks")
                .dim()
                .into()
        } else if area.width < 80 && state.opened.is_some() {
            key_hints(&[("h/←/Esc", "feed"), ("r", "reply"), ("v", "followers")])
        } else if area.width < 80 {
            key_hints(&[
                ("n", "post"),
                ("f", "follow"),
                ("v", "followers"),
                ("/", "find"),
                ("o", "topics"),
            ])
        } else {
            let mut items = vec![
                ("n", "post"),
                ("r", "reply"),
                ("f", "follow"),
                ("v", "followers"),
                ("F", "manage"),
                ("d", "details"),
                ("F9", "deliveries"),
                ("o", "topics"),
            ];
            if state.opened.is_some() {
                items.insert(0, ("h/←/Esc", "feed"));
                items.push(("[/]", "entry"));
            }
            key_hints(&items)
        };
        Paragraph::new(footer).render(row(area, area.height - 1, 1), buffer);
    }
}

fn render_feed(state: &mut BoardState, area: Rect, buffer: &mut Buffer) {
    let roots = state.roots();
    if roots.is_empty() {
        Paragraph::new(if state.query.is_empty() {
            "No conversations here yet.\nPress n to make a post; @ mentions invite an agent."
        } else {
            "No matching conversations.\nPress / to change the search."
        })
        .render(area, buffer);
        return;
    }
    let selected = roots
        .iter()
        .position(|entry| Some(&entry.conversation_id) == state.selected.as_ref())
        .unwrap_or_default();
    let total = roots.len();
    let overflow = total.saturating_mul(4) > usize::from(area.height);
    let page = usize::from(area.height.saturating_sub(u16::from(overflow)) / 4).max(1);
    let mut start = state.feed_scroll;
    if selected < start {
        start = selected;
    }
    if selected >= start + page {
        start = selected + 1 - page;
    }
    let mut lines = Vec::new();
    for root in roots.into_iter().skip(start).take(page) {
        let entries = state.entries(&root.conversation_id);
        let last = entries.last().copied().unwrap_or(root);
        let unread = state.unread(&root.conversation_id);
        let selected = Some(&root.conversation_id) == state.selected.as_ref();
        let prefix = if selected { "› " } else { "  " };
        let title = root
            .message
            .body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Post");
        let mut headline = Line::from(vec![
            prefix.cyan(),
            if unread { "● ".cyan() } else { "  ".into() },
            title.to_string().bold(),
        ]);
        if selected {
            headline = headline.style(crate::bottom_pane::active_tab_style());
        }
        lines.push(headline);
        lines.push(
            format!(
                "  {} · #{}",
                author(root),
                root.message.topic.as_deref().unwrap_or("general")
            )
            .dim()
            .into(),
        );
        lines.push(
            format!(
                "  {} replies · {}{}",
                entries.len().saturating_sub(1),
                time(last.message.created_at),
                if state.following(&root.conversation_id) {
                    " · Following"
                } else {
                    ""
                }
            )
            .dim()
            .into(),
        );
        lines.push(Line::default());
    }
    state.feed_scroll = start;
    Paragraph::new(lines).render(area, buffer);
    if overflow {
        Paragraph::new(
            format!(
                "Posts {}-{}/{total} · j/k ↑/↓ move",
                start + 1,
                (start + page).min(total)
            )
            .dim(),
        )
        .render(row(area, area.height.saturating_sub(1), 1), buffer);
    }
}

fn render_conversation(state: &mut BoardState, area: Rect, buffer: &mut Buffer) {
    let focused = state.opened.is_some() && state.composing.is_none();
    let Some(id) = state.opened.clone().or_else(|| state.selected.clone()) else {
        return;
    };
    if state.followers {
        let mut lines = vec![
            Line::from("Following this conversation · F manage agents".bold()),
            Line::default(),
        ];
        for follower in state
            .snapshot
            .followers
            .iter()
            .filter(|follower| follower.conversation_id == id)
        {
            let name = if follower.identity == TeamBoardIdentity::User {
                "You · Human".into()
            } else {
                let available = state
                    .snapshot
                    .members
                    .iter()
                    .find(|member| member.author.identity == follower.identity)
                    .map_or("left team", |member| {
                        if member.available {
                            "available"
                        } else {
                            "unavailable"
                        }
                    });
                format!(
                    "{} · {} · {available}",
                    follower.author.name,
                    follower.author.machine_name.as_deref().unwrap_or("Agent")
                )
            };
            lines.push(
                format!(
                    "{name} · {}",
                    super::subscriptions::mode_label(follower.mode)
                )
                .into(),
            );
        }
        if lines.len() == 2 {
            lines.push("No followers yet · f follows as You".dim().into());
        }
        Paragraph::new(lines).render(area, buffer);
        return;
    }
    let followers = state
        .snapshot
        .followers
        .iter()
        .filter(|follower| follower.conversation_id == id)
        .count();
    let direct = state.subscription_status(
        &TeamBoardSubscriptionTarget::Conversation {
            conversation_id: id.clone(),
        },
        &TeamBoardIdentity::User,
    );
    let topic_status = state
        .snapshot
        .entries
        .iter()
        .find(|entry| entry.message.id == id)
        .and_then(|entry| entry.message.topic.as_ref())
        .filter(|topic| {
            state.subscription_mode(
                &TeamBoardSubscriptionTarget::Topic {
                    topic: (*topic).clone(),
                },
                &TeamBoardIdentity::User,
            ) != TeamBoardSubscriptionMode::Off
        })
        .map_or(String::new(), |topic| format!(" · Watching #{topic}"));
    Paragraph::new(
        format!(
            "{} · {direct}{topic_status} · {followers} followers",
            if focused { "Conversation" } else { "Preview" }
        )
        .cyan(),
    )
    .render(row(area, 0, 1), buffer);
    let content = row(area, 1, area.height.saturating_sub(2));
    if content.is_empty() {
        return;
    }
    let boundary = state
        .unread_boundary
        .get(&id)
        .copied()
        .unwrap_or_else(|| state.read_seq(&id));
    let entries = state.entries(&id);
    let mut lines = Vec::new();
    let mut sources = Vec::new();
    let mut positions = Vec::new();
    let mut divider = false;
    for entry in &entries {
        if focused
            && !divider
            && entry.append_seq > boundary
            && entry.author.identity != TeamBoardIdentity::User
        {
            lines.push(Line::from("── Unread replies ──".cyan()));
            sources.push((0, 0));
            divider = true;
        }
        let start = lines.len();
        lines.push(Line::from(vec![
            if state.reply_targets.get(&id) == Some(&entry.message.id) {
                "› ".cyan()
            } else {
                "".into()
            },
            author(entry).bold(),
            format!("  {}", time(entry.message.created_at)).dim(),
        ]));
        sources.push((0, 0));
        if let Some(reply_to) = &entry.message.reply_to {
            let name = entries
                .iter()
                .find(|target| &target.message.id == reply_to)
                .map_or("earlier entry", |target| target.author.name.as_str());
            lines.push(format!("↳ replying to {name}").dim().into());
            sources.push((1, 0));
        }
        // Keep source positions so rewrapping retains the text at the top of the viewport.
        for (index, text) in entry.message.body.split('\n').enumerate() {
            let line = Line::from(text);
            for wrapped in word_wrap_line_with_source(&line, usize::from(content.width)) {
                sources.push((index + 2, wrapped.range.start));
                lines.push(line_to_static(&wrapped.line));
            }
        }
        positions.push((
            entry.message.id.clone(),
            entry.append_seq,
            start,
            lines.len(),
        ));
        lines.push(Line::default());
        sources.push((usize::MAX, 0));
    }
    let latest = entries.last().map_or(0, |entry| entry.append_seq);
    let line_count = lines.len();
    let max_scroll = line_count.saturating_sub(usize::from(content.height));
    let anchored = state.scroll_anchor.as_ref().and_then(|anchor| {
        positions
            .iter()
            .find(|(entry_id, _, _, _)| entry_id == &anchor.entry)
            .and_then(|(_, _, start, end)| {
                sources[*start..=*end]
                    .iter()
                    .rposition(|(line, byte)| *line == anchor.source_line && *byte <= anchor.byte)
                    .map(|offset| start + offset)
            })
    });
    let scroll = if focused {
        if state.at_bottom {
            max_scroll
        } else if state.scroll_width != 0 && state.scroll_width != content.width {
            anchored.unwrap_or(state.scroll).min(max_scroll)
        } else {
            state.scroll.min(max_scroll)
        }
    } else {
        0
    };
    Paragraph::new(
        lines
            .into_iter()
            .skip(scroll)
            .take(usize::from(content.height))
            .collect::<Vec<_>>(),
    )
    .render(content, buffer);
    let mut new_replies = 0;
    if focused {
        state.scroll = scroll;
        state.scroll_width = content.width;
        // Reflow may start earlier on the same source line. Retain the original byte until
        // navigation changes it, so repeated width changes cannot drift toward the beginning.
        if state.at_bottom || anchored.is_none() {
            state.scroll_anchor = positions
                .iter()
                .find(|(_, _, start, end)| *start <= scroll && *end >= scroll)
                .map(|(id, _, _, _)| ScrollAnchor {
                    entry: id.clone(),
                    source_line: sources[scroll].0,
                    byte: sources[scroll].1,
                });
        }
        state.at_bottom = scroll == max_scroll;
        let seen = state.latest_seen.entry(id.clone()).or_insert(latest);
        if state.at_bottom {
            *seen = latest;
        }
        new_replies = positions
            .iter()
            .filter(|(_, seq, _, _)| *seq > *seen)
            .count();
        state.displayed = Some(DisplayedEntries {
            conversation: id,
            width: content.width,
            start: scroll,
            end: (scroll + usize::from(content.height)).min(line_count),
            entries: positions
                .into_iter()
                .map(|(_, seq, _, end)| (seq, end))
                .collect(),
        });
    }
    let status = if focused {
        let above = if scroll > 0 { " ↑" } else { "" };
        let below = if scroll < max_scroll { " ↓" } else { "" };
        let position = format!(
            "Lines {}-{}/{line_count}{above}{below}",
            scroll + 1,
            (scroll + usize::from(content.height)).min(line_count)
        );
        if new_replies > 0 {
            format!("{position} · {new_replies} new")
        } else {
            position
        }
    } else {
        "Preview does not mark replies read · l/Enter opens conversation".into()
    };
    Paragraph::new(status.dim()).render(row(area, area.height.saturating_sub(1), 1), buffer);
}
