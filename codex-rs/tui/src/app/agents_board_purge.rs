//! Human Board cleanup previews a fixed revision before a durable purge submission.

use super::agents_operator_operations::OperationHeader;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::TeamBoardPurgePreview;
use codex_app_server_protocol::TeamBoardPurgeScope;
use codex_team::TeamRequest;
use codex_team::TeamResponse;
use ratatui::style::Stylize;

const PREVIEW_VIEW: &str = "agents-board-purge-preview";

#[derive(Debug)]
pub(crate) enum PurgeAction {
    Preview(TeamBoardPurgeScope),
    CancelPreview(Uuid),
    Ready {
        request_id: Uuid,
        result: Result<TeamBoardPurgePreview, String>,
    },
    Confirm(TeamBoardPurgePreview),
}

pub(super) fn scope_label(scope: &TeamBoardPurgeScope) -> String {
    match scope {
        TeamBoardPurgeScope::Conversation { .. } => "Selected conversation".into(),
        TeamBoardPurgeScope::Topic { topic } => format!("Topic #{topic}"),
        TeamBoardPurgeScope::All => "Entire shared Board".into(),
    }
}

impl App {
    pub(super) fn open_agents_board_actions(&mut self) {
        let state = self
            .agents_hosts
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A topic browser selection must not expose deletion of a hidden feed selection.
        let conversation = (!state.topics_open)
            .then(|| state.focused_conversation().cloned())
            .flatten();
        let topic = if state.topics_open {
            state.selected_topic.clone()
        } else {
            state.topic_filter.clone().or_else(|| {
                conversation.as_ref().and_then(|id| {
                    state
                        .snapshot
                        .entries
                        .iter()
                        .find(|entry| entry.message.id == *id)
                        .and_then(|entry| entry.message.topic.clone())
                })
            })
        };
        drop(state);

        let mut items = vec![SelectionItem {
            name: "Delivery diagnostics".into(),
            actions: vec![Box::new(|sender| {
                sender.unscoped().send(AppEvent::AgentsBoardDiagnostics)
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];
        for (name, scope) in [
            (
                "Delete conversation",
                conversation
                    .map(|conversation_id| TeamBoardPurgeScope::Conversation { conversation_id }),
            ),
            (
                "Clear topic",
                topic.map(|topic| TeamBoardPurgeScope::Topic { topic }),
            ),
            ("Clear Board", Some(TeamBoardPurgeScope::All)),
        ] {
            let description = scope
                .as_ref()
                .map(scope_label)
                .unwrap_or_else(|| "Select a conversation or topic first".into());
            items.push(SelectionItem {
                name: name.into(),
                description: Some(description),
                is_disabled: scope.is_none(),
                disabled_reason: scope.is_none().then(|| "No selection".into()),
                actions: vec![Box::new(move |sender| {
                    if let Some(scope) = scope.clone() {
                        sender.unscoped().send(AppEvent::AgentsBoardPurge(Box::new(
                            PurgeAction::Preview(scope),
                        )));
                    }
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.push(SelectionItem {
            name: "Cleanup and member operation progress".into(),
            actions: vec![Box::new(|sender| {
                sender.unscoped().send(AppEvent::AgentsOperatorOperations)
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Board actions".into()),
            subtitle: Some(
                "Deletion previews the affected conversations before confirmation.".into(),
            ),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn handle_agents_board_purge(&mut self, action: PurgeAction) {
        match action {
            PurgeAction::Preview(scope) => {
                let Some(client) = self.agents_hosts.client.clone() else {
                    self.agents_hosts.notice =
                        Some("The shared team coordinator is unavailable".into());
                    return;
                };
                let request_id = Uuid::new_v4();
                self.agents_hosts.purge_preview_request = Some(request_id);
                self.chat_widget.show_selection_view(SelectionViewParams {
                    view_id: Some(PREVIEW_VIEW),
                    title: Some("Preparing Board cleanup preview".into()),
                    subtitle: Some(scope_label(&scope)),
                    on_cancel: Some(Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::AgentsBoardPurge(Box::new(
                            PurgeAction::CancelPreview(request_id),
                        )))
                    })),
                    ..SelectionViewParams::picker()
                });
                let sender = self.app_event_tx.unscoped();
                self.agents_hosts.spawn(async move {
                    let result = match client
                        .request(TeamRequest::BoardPurgePreview { scope })
                        .await
                    {
                        Ok(TeamResponse::BoardPurgePreview { preview }) => Ok(preview),
                        Ok(_) => {
                            Err("The coordinator returned an unexpected cleanup preview".into())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    sender.send(AppEvent::AgentsBoardPurge(Box::new(PurgeAction::Ready {
                        request_id,
                        result,
                    })));
                });
            }
            PurgeAction::CancelPreview(id) => {
                if self.agents_hosts.purge_preview_request == Some(id) {
                    self.agents_hosts.purge_preview_request = None;
                }
            }
            PurgeAction::Ready { request_id, result } => {
                if self.agents_hosts.purge_preview_request != Some(request_id) {
                    return;
                }
                self.agents_hosts.purge_preview_request = None;
                let params = match result {
                    Ok(preview) => purge_confirmation(preview),
                    Err(error) => SelectionViewParams {
                        title: Some("Board cleanup preview unavailable".into()),
                        subtitle: Some(error),
                        ..SelectionViewParams::picker()
                    },
                };
                self.chat_widget
                    .replace_selection_view_if_present(PREVIEW_VIEW, params);
            }
            PurgeAction::Confirm(preview) => {
                if preview.conversation_count == 0 {
                    return;
                }
                self.agents_hosts.notice = Some(
                    "Submitting Board cleanup · track worker acknowledgments in Team operations"
                        .into(),
                );
                self.submit_agents_team_request(TeamRequest::BoardPurge {
                    scope: preview.scope,
                    expected_revision: preview.revision,
                });
            }
        }
    }
}

fn purge_confirmation(preview: TeamBoardPurgePreview) -> SelectionViewParams {
    let empty = preview.conversation_count == 0;
    let header = OperationHeader(vec![
        scope_label(&preview.scope).bold().into(),
        format!(
            "{} conversations · {} posts and replies",
            preview.conversation_count, preview.message_count
        )
        .into(),
        "Permanently removes content from the Board and managed worker copies.".into(),
        "Existing session histories remain. There is no restore."
            .red()
            .into(),
        "Offline machines finish cleanup after reconnecting."
            .dim()
            .into(),
    ]);
    SelectionViewParams {
        title: Some("Confirm permanent Board cleanup".into()),
        header: Box::new(header),
        items: vec![
            SelectionItem {
                name: "Cancel".into(),
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: if empty {
                    "Nothing to delete"
                } else {
                    "Permanently delete these conversations"
                }
                .into(),
                is_disabled: empty,
                disabled_reason: empty.then(|| "The selected scope is empty".into()),
                actions: vec![Box::new(move |sender| {
                    sender.unscoped().send(AppEvent::AgentsBoardPurge(Box::new(
                        PurgeAction::Confirm(preview.clone()),
                    )))
                })],
                dismiss_on_select: true,
                require_explicit_confirmation: true,
                ..Default::default()
            },
        ],
        initial_selected_idx: Some(0),
        ..SelectionViewParams::picker()
    }
}

#[cfg(test)]
#[path = "agents_board_purge_tests.rs"]
mod tests;
