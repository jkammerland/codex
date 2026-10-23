//! Shared rename editing and task shortcuts respect configured editor and list bindings.

use super::*;
use crossterm::event::KeyEventKind;

impl AgentsOverviewView {
    pub(in crate::app::agents_overview_view) fn center_back(&mut self) {
        let mut state = self.state();
        if state.editing_metadata() || state.help {
            drop(state);
            self.on_ctrl_c();
        } else if state.hosts_focused || state.filters_focused {
            state.hosts_focused = false;
            state.filters_focused = false;
        } else if state.host_filter.take().is_some() {
            state.scroll = 0;
            drop(state);
            self.reconcile_command_center_selection();
        } else if state.removal.is_none() && !state.overview_required {
            state.completion = Some(ViewCompletion::Cancelled);
        }
    }

    pub(in crate::app::agents_overview_view) fn rename_key(&mut self, key: KeyEvent) -> bool {
        let mut state = self.state();
        if state.rename_target.is_none() {
            return false;
        }
        let input = &mut state.input;
        let editing = input.should_handle_vim_insert_escape(key) || input.is_vim_operator_pending();
        if !editing {
            match self.keymap.action_for(key) {
                Some(ListAction::Cancel) => {
                    drop(state);
                    self.on_ctrl_c();
                    return true;
                }
                Some(ListAction::Accept) if !is_plain_text_key_event(key) => {
                    let online = (state.connection_notice.is_none() || state.machines_enabled)
                        && !state.creating_worktree;
                    drop(state);
                    if online {
                        self.activate();
                    }
                    return true;
                }
                _ => {}
            }
        }
        let editor_binding = crate::keymap::keymap_action_ids()
            .filter(|action| action.context == input.keymap_context())
            .any(|action| {
                crate::keymap::bindings_for_action(
                    &self.editor_keymap,
                    action.context.config_name(),
                    action.action,
                )
                .is_some_and(|bindings| bindings.is_pressed(key))
            });
        if editing
            || is_plain_text_key_event(key)
            || editor_binding
            || !self.center_shortcut_keys.is_pressed(key)
        {
            input.input(key);
            return !crate::keymap::is_dispatch_token_event(key);
        }
        false
    }

    pub(in crate::app::agents_overview_view) fn command_center_key(
        &mut self,
        key: KeyEvent,
    ) -> bool {
        if self.state().editing_metadata() {
            if self.keymap.action_for(key) == Some(ListAction::Cancel) {
                self.on_ctrl_c();
                return true;
            }
            return false;
        }
        let mut state = self.state();
        if state.help && self.keymap.action_for(key) == Some(ListAction::Cancel) {
            state.help = false;
            return true;
        }
        if state.help {
            if key.code == KeyCode::Char('?')
                && key.kind == KeyEventKind::Press
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                state.help = false;
            }
            return true;
        }
        if crate::key_hint::ctrl(KeyCode::Char('d')).is_press(key) {
            self.app_event_tx
                .send(AppEvent::Exit(crate::app_event::ExitMode::Immediate));
            return true;
        }
        if self.center_shortcut_keys.is_pressed(key) {
            // Configured task actions win over fixed browser shortcuts, while list actions
            // apply to the currently focused region below.
            if self.keymap.action_for(key).is_none()
                || (!state.hosts_focused && !state.filters_focused)
            {
                return false;
            }
        } else if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('b') => {
                    self.app_event_tx.send(AppEvent::AgentsTeamBoard);
                    return true;
                }
                KeyCode::Char('q') => {
                    self.app_event_tx
                        .send(AppEvent::Exit(crate::app_event::ExitMode::Immediate));
                    return true;
                }
                KeyCode::Char('h') => {
                    drop(state);
                    self.center_back();
                    return true;
                }
                KeyCode::Char('l') => {
                    if state.hosts_focused || state.filters_focused {
                        state.hosts_focused = false;
                        state.filters_focused = false;
                    } else {
                        let available = (state.connection_notice.is_none()
                            || state.machines_enabled)
                            && !state.creating_worktree
                            && state.removal.is_none();
                        drop(state);
                        if available {
                            self.activate();
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Char('?')
            && !self.center_shortcut_keys.is_pressed(key)
            && key.kind == KeyEventKind::Press
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            state.help = !state.help;
            return true;
        }
        if key.code == KeyCode::Tab
            && !self.center_shortcut_keys.is_pressed(key)
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            let count = if state.hosts_visible { 3 } else { 2 };
            let current = if state.hosts_focused && state.hosts_visible {
                2
            } else if state.filters_focused {
                0
            } else {
                1
            };
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                count - 1
            } else {
                1
            };
            let next = (current + step) % count;
            state.hosts_focused = next == 2;
            state.filters_focused = next == 0;
            return true;
        }
        let action = self.keymap.action_for(key);
        if state.hosts_focused || state.filters_focused {
            match action {
                Some(ListAction::Cancel | ListAction::MoveLeft) => {
                    drop(state);
                    self.center_back();
                    return true;
                }
                Some(ListAction::Accept | ListAction::MoveRight) if state.hosts_focused => {
                    state.hosts_focused = false;
                    return true;
                }
                Some(ListAction::Accept) => {
                    state.filters_focused = false;
                    return true;
                }
                Some(
                    action @ (ListAction::MoveUp
                    | ListAction::MoveDown
                    | ListAction::MoveRight
                    | ListAction::JumpTop
                    | ListAction::JumpBottom),
                ) => {
                    let forward = matches!(action, ListAction::MoveDown | ListAction::MoveRight);
                    if state.hosts_focused {
                        let index = state
                            .host_filter
                            .as_ref()
                            .and_then(|key| {
                                state
                                    .hosts
                                    .iter()
                                    .position(|host| &host.key == key)
                                    .map(|index| index + 1)
                            })
                            .unwrap_or_default();
                        let last = state.hosts.len();
                        let next = match action {
                            ListAction::JumpTop => 0,
                            ListAction::JumpBottom => last,
                            _ if forward => (index + 1).min(last),
                            _ => index.saturating_sub(1),
                        };
                        state.host_filter = next
                            .checked_sub(1)
                            .map(|index| state.hosts[index].key.clone());
                    } else {
                        let count = TASK_FILTERS.len();
                        state.status_filter = match action {
                            ListAction::JumpTop => 0,
                            ListAction::JumpBottom => count - 1,
                            _ if forward => (state.status_filter + 1) % count,
                            _ => (state.status_filter + count - 1) % count,
                        };
                    }
                    state.scroll = 0;
                    drop(state);
                    self.reconcile_command_center_selection();
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}
