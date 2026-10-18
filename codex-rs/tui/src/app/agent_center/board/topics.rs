//! Topics reuse the shared Board projection without reading or waking conversations.

use super::*;
use codex_app_server_protocol::TeamBoardTopicOverview;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

impl BoardState {
    pub(crate) fn topics(&self) -> Vec<TeamBoardTopicOverview> {
        codex_team::topic_overviews(&self.snapshot, &TeamBoardIdentity::User)
            .into_iter()
            .filter(|topic| {
                self.query.split_whitespace().all(|word| {
                    let word = word.trim_start_matches('#').to_lowercase();
                    topic.topic.to_lowercase().contains(&word)
                        || topic
                            .participants
                            .iter()
                            .any(|author| author.name.to_lowercase().contains(&word))
                        || topic.recent_conversations.iter().any(|conversation| {
                            conversation.root.excerpt.to_lowercase().contains(&word)
                                || conversation.latest.excerpt.to_lowercase().contains(&word)
                        })
                })
            })
            .collect()
    }

    pub(super) fn reconcile_topic_selection(&mut self) {
        let topics = self.topics();
        if !topics
            .iter()
            .any(|topic| Some(&topic.topic) == self.selected_topic.as_ref())
        {
            self.selected_topic = topics.first().map(|topic| topic.topic.clone());
            self.topic_preview_scroll = 0;
        }
    }

    pub(crate) fn show_topics(&mut self) {
        if !self.topics_open {
            self.feed_positions.insert(
                self.topic_filter.clone(),
                (self.selected.clone(), self.feed_scroll),
            );
        }
        self.topics_open = true;
        self.topic_preview_focused = false;
        self.opened = None;
        self.followers = false;
        self.reconcile_topic_selection();
    }

    pub(crate) fn open_selected_topic(&mut self) {
        if let Some(topic) = self.selected_topic.clone() {
            self.topic = topic.clone();
            self.filter = Filter::All;
            // Topic selection carries exact identity, independently of fuzzy search.
            self.query.clear();
            self.restore_feed(Some(topic));
        }
    }

    fn restore_feed(&mut self, topic: Option<String>) {
        self.topics_open = false;
        self.topic_filter = topic;
        let position = self.feed_positions.get(&self.topic_filter).cloned();
        let roots = self.roots();
        let selected = position
            .as_ref()
            .and_then(|(selected, _)| selected.clone())
            .filter(|selected| roots.iter().any(|root| root.conversation_id == *selected))
            .or_else(|| roots.first().map(|root| root.conversation_id.clone()));
        self.selected = selected;
        self.feed_scroll = position.map_or(0, |(_, scroll)| scroll);
    }
}

impl BoardView {
    pub(super) fn handle_topics_key(&self, state: &mut BoardState, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                state.topic_preview_focused = !state.topic_preview_focused;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => state.open_selected_topic(),
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('o') => {
                if state.topic_preview_focused {
                    state.topic_preview_focused = false;
                } else {
                    state.restore_feed(None);
                }
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('j')
            | KeyCode::Char('k')
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => {
                if state.topic_preview_focused {
                    let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        state.topic_preview_page_height.max(1)
                    } else {
                        1
                    };
                    state.topic_preview_scroll = match key.code {
                        KeyCode::Home => 0,
                        KeyCode::End => state.topic_preview_max_scroll,
                        KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                            state.topic_preview_scroll.saturating_sub(step)
                        }
                        _ => state
                            .topic_preview_scroll
                            .saturating_add(step)
                            .min(state.topic_preview_max_scroll),
                    };
                    return true;
                }
                let topics = state.topics();
                let index = topics
                    .iter()
                    .position(|topic| Some(&topic.topic) == state.selected_topic.as_ref())
                    .unwrap_or_default();
                let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                    state.page_height.max(1)
                } else {
                    1
                };
                let next = match key.code {
                    KeyCode::Home => 0,
                    KeyCode::End => topics.len().saturating_sub(1),
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                        index.saturating_sub(step)
                    }
                    _ => index
                        .saturating_add(step)
                        .min(topics.len().saturating_sub(1)),
                };
                state.selected_topic = topics.get(next).map(|topic| topic.topic.clone());
                if next != index {
                    state.topic_preview_scroll = 0;
                }
            }
            KeyCode::Char('f') => {
                if let Some(topic) = state.selected_topic.clone() {
                    let target = TeamBoardSubscriptionTarget::Topic { topic };
                    let mode = if state.subscription_mode(&target, &TeamBoardIdentity::User)
                        == TeamBoardSubscriptionMode::Off
                    {
                        TeamBoardSubscriptionMode::Watch
                    } else {
                        TeamBoardSubscriptionMode::Off
                    };
                    self.request(TeamBoardParams::SubscriptionSet {
                        target,
                        identity: None,
                        mode,
                    });
                }
            }
            KeyCode::Char('F') | KeyCode::Char('v') => {
                if let Some(topic) = state.selected_topic.clone() {
                    self.sender.send(AppEvent::AgentsBoardManageTopic(topic));
                }
            }
            // Conversation-only commands have no target in the topic browser.
            KeyCode::Char('r') | KeyCode::Char('d') | KeyCode::Char('[') | KeyCode::Char(']') => {}
            _ => return false,
        }
        true
    }
}

pub(super) fn render_topics(state: &mut BoardState, area: Rect, buffer: &mut Buffer) {
    let topics = state.topics();
    if topics.is_empty() {
        Paragraph::new("No matching topics.\nPress n to post, or / to change the search.")
            .render(area, buffer);
        return;
    }
    let (list, details) = if area.width >= 100 {
        let [list, _, details] = Layout::horizontal([
            Constraint::Percentage(44),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(area);
        (list, details)
    } else if state.topic_preview_focused {
        (Rect::default(), area)
    } else {
        (area, Rect::default())
    };
    let selected = topics
        .iter()
        .position(|topic| Some(&topic.topic) == state.selected_topic.as_ref())
        .unwrap_or_default();
    let page = usize::from(list.height.saturating_sub(1) / 5).max(1);
    let mut start = state.topics_scroll;
    if selected < start {
        start = selected;
    } else if selected >= start + page {
        start = selected + 1 - page;
    }
    state.topics_scroll = start;
    state.page_height = page;
    let mut lines = Vec::new();
    for (index, topic) in topics.iter().enumerate().skip(start).take(page) {
        let target = TeamBoardSubscriptionTarget::Topic {
            topic: topic.topic.clone(),
        };
        let mut heading = Line::from(vec![
            if index == selected {
                "› ".cyan()
            } else {
                "  ".into()
            },
            format!("#{}", topic.topic).bold(),
            format!(
                " · {}",
                state.subscription_status(&target, &TeamBoardIdentity::User)
            )
            .dim(),
        ]);
        if index == selected {
            heading = heading.style(crate::bottom_pane::active_tab_style());
        }
        lines.push(heading);
        lines.push(
            format!(
                "  {} conversations · {} unread · {} participants",
                topic.conversation_count,
                topic.unread_count,
                topic.participants.len()
            )
            .dim()
            .into(),
        );
        lines.push(
            format!(
                "  {}",
                topic
                    .latest
                    .as_ref()
                    .map_or("No posts yet", |entry| entry.excerpt.as_str())
            )
            .into(),
        );
        lines.push(topic.latest.as_ref().map_or_else(Line::default, |entry| {
            format!(
                "  {} · {}",
                entry.author.name,
                super::render::time(entry.created_at)
            )
            .dim()
            .into()
        }));
        lines.push(Line::default());
    }
    let lines = lines
        .into_iter()
        .map(|line| {
            crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                line,
                usize::from(list.width),
            )
        })
        .collect::<Vec<_>>();
    Paragraph::new(lines).render(list, buffer);
    if topics.len() > page && list.height > 0 {
        Paragraph::new(
            format!(
                "Topics {}-{}/{} · j/k move",
                start + 1,
                (start + page).min(topics.len()),
                topics.len()
            )
            .dim(),
        )
        .render(Rect::new(list.x, list.bottom() - 1, list.width, 1), buffer);
    }
    if details.is_empty() {
        return;
    }
    let topic = &topics[selected];
    let mut lines = vec![
        Line::from(format!("#{} · topic preview", topic.topic).bold()),
        Line::from("Watch: Following + unread · Wake: new posts only".dim()),
        Line::from("Previews do not mark conversations read".dim()),
        Line::default(),
        Line::from("Participants".bold()),
    ];
    for author in &topic.participants {
        lines.push(
            format!(
                "{} · {}",
                if author.identity == TeamBoardIdentity::User {
                    "You"
                } else {
                    &author.name
                },
                author.machine_name.as_deref().unwrap_or("Human")
            )
            .into(),
        );
    }
    lines.extend([Line::default(), Line::from("Recent conversations".bold())]);
    for conversation in &topic.recent_conversations {
        lines.push(conversation.root.excerpt.clone().into());
        lines.push(
            format!(
                "{} replies · {}",
                conversation.reply_count, conversation.latest.author.name
            )
            .dim()
            .into(),
        );
    }
    lines.extend([Line::default(), Line::from("Subscriptions".bold())]);
    for subscription in state
        .snapshot
        .topic_subscriptions
        .iter()
        .filter(|subscription| subscription.topic == topic.topic)
    {
        let name = if subscription.identity == TeamBoardIdentity::User {
            "You".into()
        } else {
            state
                .snapshot
                .members
                .iter()
                .find(|member| member.author.identity == subscription.identity)
                .map(|member| member.author.name.clone())
                .unwrap_or_else(|| match &subscription.identity {
                    TeamBoardIdentity::Agent { address } => format!(
                        "{} · {}/{}",
                        address.thread_id, address.machine_id, address.server_id
                    ),
                    TeamBoardIdentity::User => "You".into(),
                })
        };
        lines.push(
            format!(
                "{name} · {}",
                super::subscriptions::mode_label(subscription.mode)
            )
            .into(),
        );
    }
    let lines = crate::wrapping::word_wrap_lines(&lines, usize::from(details.width));
    let height = usize::from(details.height.saturating_sub(1));
    state.topic_preview_page_height = height.max(1);
    state.topic_preview_max_scroll = lines.len().saturating_sub(height);
    state.topic_preview_scroll = state
        .topic_preview_scroll
        .min(state.topic_preview_max_scroll);
    let end = (state.topic_preview_scroll + height).min(lines.len());
    Paragraph::new(lines[state.topic_preview_scroll..end].to_vec()).render(
        Rect {
            height: details.height.saturating_sub(1),
            ..details
        },
        buffer,
    );
    let hint = if state.topic_preview_focused {
        "j/k scroll · Tab topics"
    } else {
        "Tab scroll preview"
    };
    Paragraph::new(
        format!(
            "{}-{end}/{} · {hint}",
            state.topic_preview_scroll + 1,
            lines.len()
        )
        .dim(),
    )
    .render(
        Rect::new(details.x, details.bottom() - 1, details.width, 1),
        buffer,
    );
}
