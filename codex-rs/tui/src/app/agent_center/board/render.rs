use super::*;
use crate::bottom_pane::render_filled_tab_bar;
use crate::render::renderable::Renderable;
use crate::wrapping::word_wrap_lines;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
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

fn author(entry: &TeamBoardEntry) -> String {
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

fn time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |time| time.format("%b %d %H:%M").to_string(),
    )
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
        Paragraph::new(Line::from(vec![
            "Agent command center".bold(),
            "   t Tasks   ".dim(),
            "b Board".cyan().bold(),
        ]))
        .render(row(area, 0, 1), buffer);
        if area.height < 4 {
            return;
        }
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
        let subtitle = if state.editing_topic {
            format!("New post topic › #{}", state.topic)
        } else if state.searching {
            format!("Search › {}", state.query)
        } else if !state.query.is_empty() {
            format!("Search: {}  · / edit · #topic filters topics", state.query)
        } else {
            "Posts and conversations · / search · #topic in search · # set post topic".into()
        };
        Paragraph::new(subtitle.dim()).render(row(area, 2, 1), buffer);
        let composer_area = self.composer_area(&state, area);
        let body_end = if composer_area.height > 0 {
            composer_area.y
        } else {
            area.bottom().saturating_sub(1)
        };
        let body = row(area, 3, body_end.saturating_sub(area.y + 3));
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
        if let Some(composer) = state
            .composer
            .as_ref()
            .filter(|_| !composer_area.is_empty())
        {
            composer.render(composer_area, buffer);
        }
        let footer = if state.composing.is_some() {
            state.notice.clone().unwrap_or_else(|| {
                "@ mention · Enter send · Esc keep draft · F6 pause · F7 resume · F8 stop".into()
            })
        } else if state.followers {
            "Named followers · l/Esc close · t Tasks".into()
        } else if let Some(notice) = &state.notice {
            format!("{notice} · n post · r reply · f follow · l followers · t Tasks")
        } else {
            "n post · r reply · f follow · l followers · d details · F9 deliveries · Tab filter · t Tasks".into()
        };
        Paragraph::new(footer.dim()).render(row(area, area.height - 1, 1), buffer);
    }
}

fn render_feed(state: &mut BoardState, area: Rect, buffer: &mut Buffer) {
    let roots = state.roots();
    if roots.is_empty() {
        Paragraph::new(
            "No conversations here yet.\nPress n to make a post; @ mentions invite an agent.",
        )
        .render(area, buffer);
        return;
    }
    let selected = roots
        .iter()
        .position(|entry| Some(&entry.conversation_id) == state.selected.as_ref())
        .unwrap_or_default();
    let page = usize::from(area.height / 4).max(1);
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
                            "offline"
                        }
                    });
                format!(
                    "{} · {} · {available}",
                    follower.author.name,
                    follower.author.machine_name.as_deref().unwrap_or("Agent")
                )
            };
            lines.push(name.into());
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
    Paragraph::new(
        format!(
            "{} · {followers} followers · {}",
            if focused {
                "Conversation"
            } else {
                "Preview · Enter to read"
            },
            if state.following(&id) {
                "Following"
            } else {
                "f Follow conversation"
            }
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
    let mut positions = Vec::new();
    let mut divider = false;
    for entry in &entries {
        if focused
            && !divider
            && entry.append_seq > boundary
            && entry.author.identity != TeamBoardIdentity::User
        {
            lines.push(Line::from("── Unread replies ──".cyan()));
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
        if let Some(reply_to) = &entry.message.reply_to {
            let name = entries
                .iter()
                .find(|target| &target.message.id == reply_to)
                .map_or("earlier entry", |target| target.author.name.as_str());
            lines.push(format!("↳ replying to {name}").dim().into());
        }
        lines.extend(word_wrap_lines(
            entry
                .message
                .body
                .split('\n')
                .map(|line| Line::from(line.to_owned())),
            usize::from(content.width),
        ));
        positions.push((
            entry.message.id.clone(),
            entry.append_seq,
            start,
            lines.len(),
        ));
        lines.push(Line::default());
    }
    let latest = entries.last().map_or(0, |entry| entry.append_seq);
    let line_count = lines.len();
    let max_scroll = line_count.saturating_sub(usize::from(content.height));
    let anchored = state.scroll_anchor.as_ref().and_then(|(id, offset)| {
        positions
            .iter()
            .find(|(entry_id, _, _, _)| entry_id == id)
            .map(|(_, _, start, end)| start + offset.min(&end.saturating_sub(*start)))
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
        state.scroll_anchor = positions
            .iter()
            .find(|(_, _, start, end)| *start <= scroll && *end > scroll)
            .map(|(id, _, start, _)| (id.clone(), scroll - start));
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
    let status = if focused && scroll < max_scroll {
        if new_replies > 0 {
            format!("{new_replies} new replies · End goes to latest")
        } else {
            "More replies below · End goes to latest".into()
        }
    } else if focused {
        "r Reply · [/] select entry · Esc feed · l Followers · d Details".into()
    } else {
        "Preview does not mark replies read · Enter opens conversation".into()
    };
    Paragraph::new(status.dim()).render(row(area, area.height.saturating_sub(1), 1), buffer);
}
