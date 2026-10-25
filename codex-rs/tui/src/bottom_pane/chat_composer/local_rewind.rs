//! Keep rewind invocations local and validate the whole draft before consuming its command.

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

        // Only the accepted command is consumed; the shared app action requires an empty editor.
        self.stage_selected_slash_command_history(&CommandItem::Builtin(SlashCommand::Rewind));
        self.draft.textarea.set_text_clearing_elements("");
        self.draft.prompt_prefix = PromptPrefix::None;
        InputResult::Command(SlashCommand::Rewind)
    }
}
