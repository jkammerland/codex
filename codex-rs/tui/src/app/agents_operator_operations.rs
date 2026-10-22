//! Operator operation progress follows coordinator snapshots, including after reconnect.

use super::agents_board_purge::scope_label;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::render::renderable::Renderable;
use crate::wrapping::word_wrap_lines;
use codex_team::TeamMemberCreateState;
use codex_team::TeamOperatorOperation;
use codex_team::TeamSnapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Styled;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

const LIST_VIEW: &str = "agents-operator-operations";
const DETAIL_VIEW: &str = "agents-operator-operation";

pub(super) struct OperationHeader(pub(super) Vec<Line<'static>>);

impl Renderable for OperationHeader {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        Renderable::render(
            &Paragraph::new(word_wrap_lines(&self.0, usize::from(area.width))),
            area,
            buffer,
        );
    }

    fn desired_height(&self, width: u16) -> u16 {
        word_wrap_lines(&self.0, usize::from(width))
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

fn operation_id(operation: &TeamOperatorOperation) -> &str {
    match operation {
        TeamOperatorOperation::MemberCreate(operation) => &operation.operation_id,
        TeamOperatorOperation::BoardPurge(operation) => &operation.operation_id,
    }
}

fn operation_summary(operation: &TeamOperatorOperation) -> String {
    match operation {
        TeamOperatorOperation::MemberCreate(operation) => {
            let phase = match operation.phase {
                TeamMemberCreateState::Creating => "Creating",
                TeamMemberCreateState::EnrollmentPending => "Created · enrollment pending",
                TeamMemberCreateState::Created => "Created · team holds still apply",
                TeamMemberCreateState::Failed => "Failed",
                TeamMemberCreateState::NeedsInspection => "Needs inspection",
            };
            format!("{} · {phase}", operation.request.alias)
        }
        TeamOperatorOperation::BoardPurge(operation) => {
            let pending = operation
                .workers
                .iter()
                .filter(|worker| !worker.acknowledged)
                .count();
            if pending == 0 {
                format!("{} · cleanup complete", scope_label(&operation.scope))
            } else {
                format!(
                    "{} · coordinator cleared · {pending} worker cleanup pending",
                    scope_label(&operation.scope)
                )
            }
        }
    }
}

fn operation_lines(operation: &TeamOperatorOperation) -> Vec<Line<'static>> {
    let mut lines = vec![operation_summary(operation).bold().into()];
    match operation {
        TeamOperatorOperation::MemberCreate(operation) => {
            lines.extend([
                format!(
                    "Machine: {} / {}",
                    operation.request.machine_id, operation.request.server_id
                )
                .into(),
                format!("Checkout: {}", operation.request.cwd).into(),
                format!(
                    "Model: {}",
                    operation
                        .request
                        .model
                        .as_deref()
                        .unwrap_or("Machine configuration")
                )
                .into(),
                format!(
                    "Effort: {}",
                    operation.request.effort.as_deref().unwrap_or("Default")
                )
                .into(),
            ]);
            if let Some(address) = &operation.address {
                lines.push(format!("Session: {}", address.thread_id).into());
            }
            if let Some(error) = &operation.error {
                lines.push(error.clone().red().into());
            }
            lines.push(
                "Creation starts no model turn. Closing this view does not cancel it."
                    .dim()
                    .into(),
            );
        }
        TeamOperatorOperation::BoardPurge(operation) => {
            lines.push(
                format!(
                    "{} conversations · {} posts and replies",
                    operation.conversation_count, operation.message_count
                )
                .into(),
            );
            for worker in &operation.workers {
                let text = format!(
                    "{} / {} · {}",
                    worker.machine_id,
                    worker.server_id,
                    if worker.acknowledged {
                        "cleared"
                    } else {
                        "cleanup pending"
                    }
                );
                lines.push(if worker.acknowledged {
                    text.green().into()
                } else {
                    text.set_style(crate::style::status_style(
                        crate::style::StatusTone::Attention,
                    ))
                    .into()
                });
            }
            lines.push(
                "Offline workers finish after reconnecting. Existing session histories remain."
                    .dim()
                    .into(),
            );
        }
    }
    lines.push(
        format!("Operation: {}", operation_id(operation))
            .dim()
            .into(),
    );
    lines
}

impl App {
    pub(super) fn update_agents_operator_notice(&mut self, snapshot: &TeamSnapshot) {
        let changed = snapshot.operations.iter().rev().find(|operation| {
            self.agents_hosts
                .snapshot
                .as_ref()
                .and_then(|previous| {
                    previous
                        .operations
                        .iter()
                        .find(|candidate| operation_id(candidate) == operation_id(operation))
                })
                .is_none_or(|previous| operation_summary(previous) != operation_summary(operation))
        });
        if let Some(operation) = changed {
            let notice = operation_summary(operation);
            self.agents_hosts.notice = Some(notice.clone());
            self.agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .notice = Some(notice);
        }
    }

    pub(super) fn open_agents_operator_operations(&mut self) {
        self.chat_widget
            .show_selection_view(self.agents_operator_list());
    }

    fn agents_operator_list(&self) -> SelectionViewParams {
        let items = self
            .agents_hosts
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .operations
                    .iter()
                    .rev()
                    .map(|operation| {
                        let id = operation_id(operation).to_owned();
                        SelectionItem {
                            name: operation_summary(operation),
                            actions: vec![Box::new(move |sender| {
                                sender
                                    .unscoped()
                                    .send(AppEvent::AgentsOperatorOperation(id.clone()))
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        SelectionViewParams {
            view_id: Some(LIST_VIEW),
            title: Some("Team operations".into()),
            subtitle: Some(
                "Progress is retained by the coordinator and updates automatically.".into(),
            ),
            items,
            ..SelectionViewParams::picker()
        }
    }

    pub(super) fn open_agents_operator_operation(&mut self, id: String) {
        let Some(operation) = self.agents_hosts.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .operations
                .iter()
                .find(|operation| operation_id(operation) == id)
        }) else {
            self.agents_hosts.notice =
                Some("The operation is not available in the current snapshot".into());
            return;
        };
        self.chat_widget
            .show_selection_view(operation_detail(operation));
        self.agents_hosts.operator_operation = Some(id);
    }

    pub(super) fn selected_agents_operator_list_operation(&self) -> Option<String> {
        let index = self
            .chat_widget
            .selected_index_for_present_view(LIST_VIEW)?;
        self.agents_hosts
            .snapshot
            .as_ref()?
            .operations
            .iter()
            .rev()
            .nth(index)
            .map(|operation| operation_id(operation).to_owned())
    }

    pub(super) fn refresh_agents_operator_operation(&mut self, selected: Option<&str>) {
        let mut list = self.agents_operator_list();
        // New operations can insert before the current row; preserve its identity, not its index.
        list.initial_selected_idx = selected.and_then(|id| {
            self.agents_hosts
                .snapshot
                .as_ref()?
                .operations
                .iter()
                .rev()
                .position(|operation| operation_id(operation) == id)
        });
        self.chat_widget
            .replace_selection_view_if_present(LIST_VIEW, list);
        let Some(id) = self.agents_hosts.operator_operation.as_ref() else {
            return;
        };
        let Some(operation) = self.agents_hosts.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .operations
                .iter()
                .find(|operation| operation_id(operation) == id)
        }) else {
            return;
        };
        if !self
            .chat_widget
            .replace_selection_view_if_present(DETAIL_VIEW, operation_detail(operation))
        {
            self.agents_hosts.operator_operation = None;
        }
    }
}

fn operation_detail(operation: &TeamOperatorOperation) -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(DETAIL_VIEW),
        title: Some("Team operation".into()),
        header: Box::new(OperationHeader(operation_lines(operation))),
        items: vec![SelectionItem {
            name: "Close".into(),
            dismiss_on_select: true,
            ..Default::default()
        }],
        ..SelectionViewParams::picker()
    }
}

#[cfg(test)]
#[path = "agents_operator_operations_tests.rs"]
mod tests;
