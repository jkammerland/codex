//! App integration owns coordinator mutations and post-frame read acknowledgments.

use super::agents_overview_view::command_center::board::BoardView;
use super::*;
use crate::bottom_pane::ChatComposer;
use crate::bottom_pane::ChatComposerConfig;
use codex_app_server_protocol::TeamBoardParams;

impl App {
    pub(super) fn open_agents_team_board(&mut self) {
        {
            let mut state = self
                .agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = true;
            state.completion = None;
            if state.composer.is_none() {
                let mut composer = ChatComposer::new_with_config(
                    /*has_input_focus*/ true,
                    self.app_event_tx.unscoped(),
                    self.enhanced_keys_supported,
                    "Write a post or reply · @ mention an agent".into(),
                    self.local_settings.tui.disable_paste_burst.unwrap_or(false),
                    ChatComposerConfig {
                        popups_enabled: true,
                        trim_submission: false,
                        ..ChatComposerConfig::plain_text()
                    },
                );
                composer.set_keymap_bindings(&self.keymap);
                composer.set_vim_enabled(self.chat_widget.composer_is_vim_enabled());
                composer.set_board_mentions(&state.snapshot.members);
                state.composer = Some(composer);
            }
            if let Some(snapshot) = &self.agents_hosts.snapshot {
                state.update_team(snapshot);
            }
        }
        let view = Box::new(BoardView {
            state: self.agents_hosts.board.clone(),
            sender: self.app_event_tx.unscoped(),
        });
        if self
            .chat_widget
            .selected_index_for_present_view(AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
        {
            self.chat_widget
                .replace_bottom_pane_view_if_present(AGENTS_OVERVIEW_VIEW_ID, view);
        } else {
            self.chat_widget.show_bottom_pane_view(view);
        }
    }

    pub(super) fn repaint_agents_board(&mut self) {
        {
            let mut state = self
                .agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(snapshot) = &self.agents_hosts.snapshot {
                state.update_team(snapshot);
            }
        }
        self.chat_widget.replace_bottom_pane_view_if_present(
            AGENTS_OVERVIEW_VIEW_ID,
            Box::new(BoardView {
                state: self.agents_hosts.board.clone(),
                sender: self.app_event_tx.unscoped(),
            }),
        );
    }

    pub(super) fn open_agents_board_followers(&mut self, conversation_id: String) {
        self.open_agents_board_subscriptions(
            codex_app_server_protocol::TeamBoardSubscriptionTarget::Conversation {
                conversation_id,
            },
        );
    }

    pub(super) fn acknowledge_agents_board_frame(&mut self, focused: bool) {
        let displayed = {
            let mut state = self
                .agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let focused = focused
                && self
                    .chat_widget
                    .selected_index_for_active_view(AGENTS_OVERVIEW_VIEW_ID)
                    .is_some();
            state.take_frame_read(focused)
        };
        if let Some((conversation_id, append_seq)) = displayed {
            let operation = codex_team::TeamOperation::new(codex_team::TeamRequest::Board {
                request: TeamBoardParams::MarkRead {
                    conversation_id: conversation_id.clone(),
                    identity: None,
                    append_seq,
                },
            });
            let id = operation.operation_id.clone();
            self.agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_reads
                .insert(
                    id.clone(),
                    super::agents_overview_view::command_center::board::PendingRead {
                        conversation: conversation_id,
                        append_seq,
                        failed: false,
                        confirmed: false,
                    },
                );
            self.agents_hosts
                .team_operations
                .insert(id.clone(), operation);
            self.retry_agents_team_request(id);
        }
    }
}
