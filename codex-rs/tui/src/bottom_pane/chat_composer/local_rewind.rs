//! Keep rewind invocations local and retain the whole draft until app validation accepts it.

use super::*;

impl ChatComposer {
    pub(super) fn is_local_rewind(&self) -> bool {
        self.slash_commands_enabled()
            && self.draft.prompt_prefix != PromptPrefix::Shell
            && parse_slash_name(&self.current_text_with_pending())
                .is_some_and(|(name, _, _)| name == SlashCommand::Rewind.command())
    }

    pub(super) fn try_dispatch_local_rewind(&mut self) -> Option<InputResult> {
        self.is_local_rewind().then(|| self.dispatch_local_rewind())
    }

    pub(super) fn local_rewind_rejection(&self) -> Option<&'static str> {
        // Completion must not discard a draft suffix or hidden attachment/paste state.
        let text = self.current_text();
        // The full `/` menu has no parsed command name until its selection is accepted.
        let has_arguments = text != "/"
            && parse_slash_name(&text).is_none_or(|(name, rest, _)| {
                !rest.is_empty()
                    || (name != SlashCommand::Rewind.command()
                        && self.current_cursor() > 1
                        && self.current_cursor() < name.len() + 1)
            });
        if has_arguments
            || !self.attachments.is_empty()
            || !self.draft.pending_pastes.is_empty()
            || self.is_in_paste_burst()
        {
            return Some("Use /rewind without arguments, attachments, or pending pastes.");
        }

        if self.is_task_running || self.team_execution_held {
            return Some("'/rewind' is disabled while a task is in progress or paused.");
        }
        if self.blocks_direct_input {
            return Some("'/rewind' is unavailable in a parent-owned thread.");
        }
        if self.side_conversation_active {
            return Some(
                "'/rewind' is unavailable in side conversations. Return to the main thread first.",
            );
        }
        None
    }

    pub(super) fn reject_local_rewind(&self, message: &str) -> InputResult {
        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
            history_cell::new_error_event(message.to_string()),
        )));
        InputResult::None
    }

    pub(super) fn dispatch_local_rewind(&mut self) -> InputResult {
        if let Some(message) = self.local_rewind_rejection() {
            return self.reject_local_rewind(message);
        }

        // App-only restrictions can still reject this command. Dismiss its popup without
        // completing or clearing the original text, including a partial name or the full menu.
        let text = self.current_text();
        self.popups.dismissed_command_token = slash_input::command_popup_filter_text(
            text.lines().next().unwrap_or(""),
            /*cursor*/ 0,
        );
        self.popups.active = ActivePopup::None;
        InputResult::Command(SlashCommand::Rewind)
    }
}

impl super::super::BottomPane {
    pub(crate) fn accept_rewind_command(&mut self) {
        self.composer
            .stage_selected_slash_command_history(&CommandItem::Builtin(SlashCommand::Rewind));
        self.composer.record_pending_slash_command_history();
        self.composer.draft.textarea.set_text_clearing_elements("");
        self.composer.draft.prompt_prefix = PromptPrefix::None;
        self.composer.sync_popups();
        self.request_redraw();
    }
}

#[cfg(test)]
#[path = "local_rewind_tests.rs"]
mod tests;
