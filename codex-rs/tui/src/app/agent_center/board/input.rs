use super::*;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::keymap::KeymapContextSet;
use codex_app_server_protocol::TeamBoardMention;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;

impl BottomPaneView for BoardView {
    fn view_id(&self) -> Option<&'static str> {
        Some(AGENTS_OVERVIEW_VIEW_ID)
    }
    fn selected_index(&self) -> Option<usize> {
        Some(usize::MAX)
    }
    fn accepts_input_when_disconnected(&self) -> bool {
        true
    }
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }
    fn keymap_contexts(&self) -> KeymapContextSet {
        let state = self.state();
        if state.composing.is_some() {
            state
                .composer
                .as_ref()
                .map(ChatComposer::keymap_contexts)
                .unwrap_or_default()
        } else {
            KeymapContextSet::default()
        }
    }
    fn handle_key_event(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let mut state = self.state();
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            if let Some(id) = state
                .pending
                .values()
                .find(|pending| pending.failed)
                .map(|pending| pending.operation_id.clone())
                .or_else(|| {
                    state
                        .pending_subscriptions
                        .iter()
                        .find(|(_, pending)| pending.failed)
                        .map(|(id, _)| id.clone())
                })
                .or_else(|| {
                    state
                        .pending_reads
                        .iter()
                        .find(|(_, pending)| pending.failed)
                        .map(|(id, _)| id.clone())
                })
            {
                self.sender.send(AppEvent::AgentsTeamRetry(id));
            }
            return;
        }
        if let Some(draft_key) = state.composing.clone() {
            let Some(composer) = state.composer.as_mut() else {
                return;
            };
            if key.code == KeyCode::Esc
                && !composer.popup_active()
                && !composer.should_handle_vim_insert_escape(key)
            {
                composer.flush_pending_input();
                state.save_draft();
                state.composing = None;
                return;
            }
            let before = composer.snapshot_draft_with_pending();
            let content_before = composer.recovery_snapshot();
            let (result, _) = composer.handle_key_event(key);
            if let InputResult::Submitted {
                text,
                text_elements,
            } = result
            {
                let mut bindings = composer.take_recent_submission_mention_bindings();
                // The shared composer clears on dispatch. Board retains the exact draft until ack.
                composer.restore_draft(before);
                if state.pending.contains_key(&draft_key) {
                    state.notice = Some(
                        "This draft is awaiting acknowledgment · Ctrl+R retries an unknown outcome"
                            .into(),
                    );
                    return;
                }
                let mut mentions = Vec::new();
                for element in text_elements {
                    let start = element.byte_range.start;
                    let end = element.byte_range.end;
                    let Some(display) = text.get(start..end) else {
                        continue;
                    };
                    if let Some(index) = bindings.iter().position(|binding| {
                        format!("{}{}", binding.sigil, binding.mention) == display
                    }) {
                        let binding = bindings.remove(index);
                        if let Some(identity) = binding
                            .path
                            .strip_prefix("board:")
                            .and_then(|value| serde_json::from_str(value).ok())
                        {
                            mentions.push(TeamBoardMention {
                                identity,
                                display: display.into(),
                                start: start as u32,
                                end: end as u32,
                            });
                        }
                    }
                }
                if mentions.iter().any(|mention| {
                    !state
                        .snapshot
                        .members
                        .iter()
                        .any(|member| member.author.identity == mention.identity)
                }) {
                    state.notice = Some("A mentioned agent left the team · remove that mention or select another agent".into());
                    return;
                }
                let request = if draft_key.is_empty() {
                    TeamBoardParams::Post {
                        topic: match state.topic.trim().trim_start_matches('#').trim() {
                            "" => "general".into(),
                            topic => topic.into(),
                        },
                        body: text,
                        mentions,
                    }
                } else {
                    TeamBoardParams::Reply {
                        conversation_id: draft_key.clone(),
                        reply_to: state.reply_targets.get(&draft_key).cloned(),
                        body: text,
                        mentions,
                    }
                };
                let operation = TeamOperation::new(TeamRequest::Board { request });
                let revision = state
                    .draft_revisions
                    .get(&draft_key)
                    .copied()
                    .unwrap_or_default();
                state.pending.insert(
                    draft_key,
                    PendingDraft {
                        operation_id: operation.operation_id.clone(),
                        revision,
                        failed: false,
                    },
                );
                state.notice = Some("Sending…".into());
                self.sender.send(AppEvent::AgentsBoardOperation(operation));
            } else {
                let after = composer.recovery_snapshot();
                if after.text != content_before.text
                    || after.text_elements != content_before.text_elements
                    || after.mention_bindings != content_before.mention_bindings
                    || after.pending_pastes != content_before.pending_pastes
                {
                    *state.draft_revisions.entry(draft_key).or_default() += 1;
                }
            }
            state.save_draft();
            return;
        }
        if state.searching || state.editing_topic {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    state.searching = false;
                    state.editing_topic = false;
                }
                KeyCode::Backspace => {
                    if state.editing_topic {
                        state.topic.pop();
                    } else {
                        state.query.pop();
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if state.editing_topic {
                        state.topic.push(ch);
                    } else {
                        state.query.push(ch);
                    }
                }
                _ => {}
            }
            state.reconcile_topic_selection();
            let roots = state.roots();
            if !roots
                .iter()
                .any(|entry| Some(&entry.conversation_id) == state.selected.as_ref())
            {
                state.selected = roots.first().map(|entry| entry.conversation_id.clone());
                state.feed_scroll = 0;
            }
            return;
        }
        if state.topics_open && self.handle_topics_key(&mut state, key) {
            return;
        }
        match key.code {
            KeyCode::Char('t') => {
                state.active = false;
                self.sender.send(AppEvent::OpenAgentsOverview);
            }
            KeyCode::Char('o') => state.show_topics(),
            KeyCode::Char('n') => {
                if state.topics_open
                    && let Some(topic) = state.selected_topic.clone()
                {
                    state.topic = topic;
                }
                state.compose(String::new());
            }
            KeyCode::Char('r') => {
                if let Some(id) = state.opened.clone().or_else(|| state.selected.clone()) {
                    state.compose(id);
                }
            }
            KeyCode::Char('/') => state.searching = true,
            KeyCode::Char('#') => state.editing_topic = true,
            KeyCode::Char('v') => state.followers = !state.followers,
            KeyCode::Char('F') => {
                if let Some(id) = state.opened.clone().or_else(|| state.selected.clone()) {
                    self.sender.send(AppEvent::AgentsBoardManageFollowers(id));
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if let Some(id) = state.opened.clone() {
                    let entries = state.entries(&id);
                    let index = entries
                        .iter()
                        .position(|entry| state.reply_targets.get(&id) == Some(&entry.message.id))
                        .unwrap_or_default();
                    let next = if key.code == KeyCode::Char('[') {
                        index.saturating_sub(1)
                    } else {
                        index.saturating_add(1).min(entries.len().saturating_sub(1))
                    };
                    if let Some(target) = entries.get(next).map(|entry| entry.message.id.clone()) {
                        state.reply_targets.insert(id, target);
                    }
                }
            }
            KeyCode::Char('f') => {
                if let Some(id) = state.opened.clone().or_else(|| state.selected.clone()) {
                    let enabled = !state.following(&id);
                    drop(state);
                    self.request(TeamBoardParams::SubscriptionSet {
                        target: TeamBoardSubscriptionTarget::Conversation {
                            conversation_id: id,
                        },
                        identity: None,
                        mode: if enabled {
                            TeamBoardSubscriptionMode::Watch
                        } else {
                            TeamBoardSubscriptionMode::Off
                        },
                    });
                }
            }
            KeyCode::Char('D') | KeyCode::F(9) => {
                self.sender.send(AppEvent::AgentsBoardDiagnostics)
            }
            KeyCode::Char('d') => {
                if let Some(id) = state.opened.clone().or_else(|| state.selected.clone()) {
                    let target = state.reply_targets.get(&id).cloned().unwrap_or(id);
                    self.sender.send(AppEvent::AgentsTeamMessage(target));
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.filter = match state.filter {
                    Filter::All => Filter::Following,
                    Filter::Following => Filter::Mentions,
                    Filter::Mentions => Filter::All,
                };
                state.selected = state
                    .roots()
                    .first()
                    .map(|entry| entry.conversation_id.clone());
                state.feed_scroll = 0;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if state.opened.is_none() => {
                state.open_selected();
                if let Some(id) = state
                    .pending_reads
                    .iter()
                    .find(|(_, pending)| {
                        pending.failed && Some(&pending.conversation) == state.opened.as_ref()
                    })
                    .map(|(id, _)| id.clone())
                {
                    if let Some(pending) = state.pending_reads.get_mut(&id) {
                        pending.failed = false;
                    }
                    self.sender.send(AppEvent::AgentsTeamRetry(id));
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                if state.followers {
                    state.followers = false;
                } else if state.opened.take().is_none() {
                    if state.topic_filter.is_some() {
                        state.show_topics();
                    } else if key.code == KeyCode::Esc {
                        state.active = false;
                        self.sender.send(AppEvent::OpenAgentsOverview);
                    }
                }
            }
            KeyCode::Home if state.opened.is_some() => {
                state.scroll = 0;
                state.scroll_anchor = None;
                state.at_bottom = false;
            }
            KeyCode::End if state.opened.is_some() => {
                state.scroll_anchor = None;
                state.at_bottom = true;
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('k')
            | KeyCode::Char('j')
            | KeyCode::PageUp
            | KeyCode::PageDown => {
                let up = matches!(key.code, KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp);
                if state.opened.is_some() {
                    let step = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        state.page_height.max(1)
                    } else {
                        1
                    };
                    state.scroll = if up {
                        state.scroll.saturating_sub(step)
                    } else {
                        state.scroll.saturating_add(step)
                    };
                    state.scroll_anchor = None;
                    state.at_bottom = false;
                } else {
                    let ids: Vec<_> = state
                        .roots()
                        .iter()
                        .map(|entry| entry.conversation_id.clone())
                        .collect();
                    let index = ids
                        .iter()
                        .position(|id| Some(id) == state.selected.as_ref())
                        .unwrap_or_default();
                    let next = if up {
                        index.saturating_sub(1)
                    } else {
                        index.saturating_add(1).min(ids.len().saturating_sub(1))
                    };
                    state.selected = ids.get(next).cloned();
                }
            }
            _ => {}
        }
    }
    fn handle_paste(&mut self, pasted: String) -> bool {
        let mut state = self.state();
        let Some(key) = state.composing.clone() else {
            return false;
        };
        let changed = state
            .composer
            .as_mut()
            .is_some_and(|composer| composer.handle_paste(pasted));
        if changed {
            *state.draft_revisions.entry(key).or_default() += 1;
            state.save_draft();
        }
        changed
    }
    fn flush_paste_burst_if_due(&mut self) -> bool {
        let mut state = self.state();
        state
            .composer
            .as_mut()
            .is_some_and(ChatComposer::flush_paste_burst_if_due)
    }
    fn is_in_paste_burst(&self) -> bool {
        self.state()
            .composer
            .as_ref()
            .is_some_and(ChatComposer::is_in_paste_burst)
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        let mut state = self.state();
        if let Some(composer) = state.composer.as_mut() {
            composer.flush_pending_input();
        }
        state.save_draft();
        state.composing = None;
        CancellationEvent::Handled
    }
    fn is_complete(&self) -> bool {
        self.state().completion.is_some()
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.state().completion
    }
}
