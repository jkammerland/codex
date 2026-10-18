//! Block dispatch before consuming draft/history; editing and local commands remain available.

use super::*;

fn command_allowed(command: SlashCommand, args: &str) -> bool {
    match command {
        SlashCommand::Init
        | SlashCommand::Review
        | SlashCommand::Compact
        | SlashCommand::Recap
        | SlashCommand::AutoReview
        | SlashCommand::MemoryUpdate => false,
        SlashCommand::Plan | SlashCommand::Side | SlashCommand::Btw => args.trim().is_empty(),
        SlashCommand::Goal => args.trim().is_empty(),
        SlashCommand::Voice => matches!(
            args.trim().to_ascii_lowercase().as_str(),
            "settings" | "mute" | "stop"
        ),
        _ => true,
    }
}

impl ChatComposer {
    pub(crate) fn set_team_execution_held(&mut self, held: bool) {
        self.team_execution_held = held;
    }

    pub(super) fn team_command_allowed(&self) -> bool {
        let text = self.draft.textarea.text();
        parse_slash_name(text).is_some_and(|(name, args, _)| {
            self.slash_input()
                .command(name)
                .is_some_and(|command| match command {
                    SlashCommandItem::Builtin(command) => command_allowed(command, args),
                    SlashCommandItem::ServiceTier(_) => true,
                })
        })
    }

    pub(super) fn team_popup_command_allowed(&self, command: &CommandItem) -> bool {
        let args = parse_slash_name(self.draft.textarea.text()).map_or("", |(_, args, _)| args);
        match command {
            CommandItem::Builtin(command) => command_allowed(*command, args),
            CommandItem::ServiceTier(_) => true,
        }
    }
}

#[cfg(test)]
#[path = "team_execution_tests.rs"]
mod tests;
