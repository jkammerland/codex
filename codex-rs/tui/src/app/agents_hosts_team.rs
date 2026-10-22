//! Team membership and communication are opt-in actions on ordinary overview rows.

use super::agents_hosts::HostUpdate;
use super::agents_hosts::TeamOperationFailure;
use super::agents_hosts::ThreadTarget;
use super::*;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::TextArea;
use crate::bottom_pane::TextAreaState;
use crate::bottom_pane::ViewCompletion;
use crate::render::renderable::Renderable;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamMessageKind;
use codex_team::TeamRequest;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::Widget;
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub(crate) enum TeamCompose {
    MemberAlias(super::agents_member::MemberDraft),
    MemberCheckout(super::agents_member::MemberDraft),
    Alias(ThreadTarget),
    Message(ThreadTarget, TeamMessageKind, TeamDeliveryMode),
    Topic,
    Post(String),
    Reply(String, TeamDeliveryMode),
    Subscribe(ThreadTarget),
}

impl ThreadTarget {
    pub(super) fn team_address(&self) -> TeamAddress {
        TeamAddress {
            machine_id: self.host.machine_id.clone(),
            server_id: self.host.server_id.clone(),
            thread_id: self.thread_id.to_string(),
        }
    }
}

struct ComposeView {
    title: String,
    input: TextArea,
    input_state: RefCell<TextAreaState>,
    kind: TeamCompose,
    sender: AppEventSender,
    completion: Option<ViewCompletion>,
}

impl ComposeView {
    fn heading(&self, width: u16) -> Vec<ratatui::text::Line<'static>> {
        crate::wrapping::word_wrap_lines(
            &[ratatui::text::Line::from(self.title.clone().bold())],
            usize::from(width),
        )
    }

    fn submit(&mut self) {
        let body = self.input.text().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let request = match &self.kind {
            TeamCompose::MemberAlias(draft) => {
                let mut draft = draft.clone();
                draft.request.alias = body;
                self.sender.send(AppEvent::AgentsMember(Box::new(
                    super::agents_member::MemberAction::Checkout(draft),
                )));
                self.completion = Some(ViewCompletion::Accepted);
                return;
            }
            TeamCompose::MemberCheckout(draft) => {
                let mut draft = draft.clone();
                draft.request.cwd = body;
                self.sender.send(AppEvent::AgentsMember(Box::new(
                    super::agents_member::MemberAction::LoadModels(draft),
                )));
                self.completion = Some(ViewCompletion::Accepted);
                return;
            }
            TeamCompose::Alias(target) => TeamRequest::Enroll {
                address: target.team_address(),
                alias: body,
            },
            TeamCompose::Message(target, kind, delivery) => TeamRequest::Send {
                recipient: target.team_address(),
                body,
                kind: *kind,
                reply_to: None,
                delivery: *delivery,
            },
            TeamCompose::Topic => {
                self.sender
                    .send(AppEvent::AgentsTeamCompose(TeamCompose::Post(body)));
                self.completion = Some(ViewCompletion::Accepted);
                return;
            }
            TeamCompose::Post(topic) => TeamRequest::Post {
                topic: topic.clone(),
                body,
            },
            TeamCompose::Reply(message_id, delivery) => TeamRequest::Reply {
                message_id: message_id.clone(),
                body,
                delivery: *delivery,
            },
            TeamCompose::Subscribe(target) => {
                self.sender.send(AppEvent::AgentsBoardSubscriptionMode {
                    target: codex_app_server_protocol::TeamBoardSubscriptionTarget::Topic {
                        topic: body.clone(),
                    },
                    identity: codex_app_server_protocol::TeamBoardIdentity::Agent {
                        address: target.team_address(),
                    },
                    name: body,
                });
                self.completion = Some(ViewCompletion::Accepted);
                return;
            }
        };
        self.sender.send(AppEvent::AgentsTeamRequest(request));
        self.completion = Some(ViewCompletion::Accepted);
    }
}

impl BottomPaneView for ComposeView {
    fn keymap_contexts(&self) -> crate::keymap::KeymapContextSet {
        crate::keymap::KeymapContextSet::new(self.input.keymap_context())
    }
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        self.input
            .should_handle_vim_insert_escape(KeyCode::Esc.into())
    }
    fn handle_key_event(&mut self, key: KeyEvent) {
        if self.input.should_handle_vim_insert_escape(key) || self.input.is_vim_operator_pending() {
            self.input.input(key);
            return;
        }
        match key.code {
            KeyCode::Esc => self.completion = Some(ViewCompletion::Cancelled),
            KeyCode::Enter if key.modifiers.is_empty() => self.submit(),
            _ => self.input.input(key),
        }
    }
    fn handle_paste(&mut self, pasted: String) -> bool {
        self.input.insert_str(&pasted);
        true
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completion = Some(ViewCompletion::Cancelled);
        CancellationEvent::Handled
    }
    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }
}

impl Renderable for ComposeView {
    fn desired_height(&self, width: u16) -> u16 {
        self.input
            .desired_height(width)
            .saturating_add(self.heading(width).len() as u16)
            .saturating_add(1)
    }
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        if area.height < 2 {
            return;
        }
        let heading = self.heading(area.width);
        let header_height = (heading.len() as u16).min(area.height.saturating_sub(1));
        Paragraph::new(heading)
            .render(Rect::new(area.x, area.y, area.width, header_height), buffer);
        let input = Rect::new(
            area.x,
            area.y + header_height,
            area.width,
            area.height.saturating_sub(header_height + 1),
        );
        StatefulWidgetRef::render_ref(
            &&self.input,
            input,
            buffer,
            &mut self.input_state.borrow_mut(),
        );
        let hint = if matches!(
            self.kind,
            TeamCompose::MemberAlias(_) | TeamCompose::MemberCheckout(_)
        ) {
            "Enter next · Esc cancel"
        } else {
            "Enter send · Esc cancel"
        };
        Paragraph::new(hint.dim())
            .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buffer);
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let header_height =
            (self.heading(area.width).len() as u16).min(area.height.saturating_sub(1));
        self.input.cursor_pos_with_state(
            Rect::new(
                area.x,
                area.y + header_height,
                area.width,
                area.height.saturating_sub(header_height + 1),
            ),
            *self.input_state.borrow(),
        )
    }
    fn cursor_style(&self, _area: Rect) -> crossterm::cursor::SetCursorStyle {
        if self.input.uses_vim_insert_cursor() {
            crossterm::cursor::SetCursorStyle::SteadyBar
        } else {
            crossterm::cursor::SetCursorStyle::DefaultUserShape
        }
    }
}

impl App {
    pub(super) fn open_agents_team_menu(&mut self, mut target: ThreadTarget) {
        target.host = self.agents_hosts.canonical_host(&target.host);
        let enrolled = self.agents_hosts.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .agents
                .iter()
                .any(|agent| agent.enrollment.address == target.team_address())
        });
        let mut items = self.agents_member_menu_items();
        let explain_target = target.clone();
        items.push(SelectionItem {
            name: "Action availability and restrictions".into(),
            description: Some("Inspect holds, authority, and actual pending approvals".into()),
            actions: vec![Box::new(move |sender| {
                sender
                    .unscoped()
                    .send(AppEvent::AgentsTeamExplain(explain_target.clone()))
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        let target_alias = target.clone();
        items.push(SelectionItem {
            name: if enrolled {
                "Edit team alias"
            } else {
                "Enroll task in the shared team"
            }
            .into(),
            description: Some("The alias is separate from the saved task title".into()),
            actions: vec![Box::new(move |sender| {
                sender
                    .unscoped()
                    .send(AppEvent::AgentsTeamCompose(TeamCompose::Alias(
                        target_alias.clone(),
                    )))
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        if enrolled {
            for (label, kind, delivery) in [
                (
                    "Send request · next turn",
                    TeamMessageKind::Request,
                    TeamDeliveryMode::NextTurn,
                ),
                (
                    "Send request · safe boundary",
                    TeamMessageKind::Request,
                    TeamDeliveryMode::SafeBoundary,
                ),
                (
                    "Send passive note",
                    TeamMessageKind::Note,
                    TeamDeliveryMode::NextTurn,
                ),
            ] {
                let recipient = target.clone();
                items.push(SelectionItem {
                    name: label.into(),
                    actions: vec![Box::new(move |sender| {
                        sender
                            .unscoped()
                            .send(AppEvent::AgentsTeamCompose(TeamCompose::Message(
                                recipient.clone(),
                                kind,
                                delivery,
                            )))
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
            let recipient = target.clone();
            items.push(SelectionItem {
                name: "Subscribe task to a board topic".into(),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamCompose(TeamCompose::Subscribe(
                            recipient.clone(),
                        )))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
            let address = target.team_address();
            items.push(SelectionItem {
                name: "Leave shared team".into(),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamRequest(TeamRequest::Leave {
                            address: address.clone(),
                        }))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.push(SelectionItem {
            name: "Machines and connections".into(),
            actions: vec![Box::new(|sender| {
                sender.unscoped().send(AppEvent::AgentsMachinesMenu)
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Message board and delivery receipts".into(),
            actions: vec![Box::new(|sender| {
                sender.unscoped().send(AppEvent::AgentsTeamBoard)
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Post to a board topic".into(),
            actions: vec![Box::new(|sender| {
                sender
                    .unscoped()
                    .send(AppEvent::AgentsTeamCompose(TeamCompose::Topic))
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        for id in &self.agents_hosts.failed_team_operations {
            let id = id.clone();
            items.push(SelectionItem {
                name: "Retry team action whose response was lost".into(),
                description: Some("Reuses the original operation ID".into()),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamRetry(id.clone()))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.extend(self.agents_host_model_menu_items(target));
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Shared team".into()),
            subtitle: Some(
                "F6 Pause · F7 Resume · F8 Stop. Controls apply to enrolled task families.".into(),
            ),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn open_agents_machines_menu(&mut self) {
        let mut items = Vec::new();
        if self.agents_hosts.client.is_none() {
            items.push(SelectionItem {
                name: "Reload configured machines".into(),
                description: Some("Retry the coordinator connection".into()),
                actions: vec![Box::new(|sender| {
                    sender.unscoped().send(AppEvent::AgentsHostReconnect(
                        super::agents_hosts::HostKey::default(),
                    ))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        for (key, host) in &self.agents_hosts.hosts {
            if *key == super::agents_hosts::HostKey::default() && !host.connected {
                continue;
            }
            if host.connected {
                let key = key.clone();
                items.push(SelectionItem {
                    name: format!("New task · {}", host.label),
                    description: Some("Use this server's configured defaults".into()),
                    actions: vec![Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::NewAgentsHostSession {
                            host: key.clone(),
                            cwd: None,
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
            let key = key.clone();
            items.push(SelectionItem {
                name: host.label.clone(),
                description: Some(
                    if host.connected {
                        "Connected · select to reconnect"
                    } else {
                        "Unavailable · select to retry"
                    }
                    .into(),
                ),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsHostReconnect(key.clone()))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Machines and connections".into()),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn open_agents_team_compose(&mut self, kind: TeamCompose) {
        let title = match &kind {
            TeamCompose::MemberAlias(draft) => {
                format!("Create member · alias on {}", draft.host_label)
            }
            TeamCompose::MemberCheckout(draft) => format!(
                "Create {} · existing absolute checkout path on {}",
                draft.request.alias, draft.host_label
            ),
            TeamCompose::Alias(_) => "Team alias".into(),
            TeamCompose::Message(_, TeamMessageKind::Request, TeamDeliveryMode::NextTurn) => {
                "Request · deliver at the next turn".into()
            }
            TeamCompose::Message(_, TeamMessageKind::Request, _) => {
                "Request · deliver at a safe boundary".into()
            }
            TeamCompose::Message(_, _, _) => "Passive note".into(),
            TeamCompose::Topic => "Board topic".into(),
            TeamCompose::Post(_) => "Post to board".into(),
            TeamCompose::Reply(_, TeamDeliveryMode::NextTurn) => {
                "Reply · deliver at the next turn".into()
            }
            TeamCompose::Reply(_, TeamDeliveryMode::SafeBoundary) => {
                "Reply · deliver at a safe boundary".into()
            }
            TeamCompose::Subscribe(_) => "Subscribe to topic".into(),
        };
        let mut input = TextArea::new();
        input.set_keymap_bindings(&self.keymap);
        input.set_vim_enabled(self.chat_widget.composer_is_vim_enabled());
        self.chat_widget
            .show_bottom_pane_view(Box::new(ComposeView {
                title,
                input,
                input_state: RefCell::default(),
                kind,
                sender: self.app_event_tx.unscoped(),
                completion: None,
            }));
    }

    pub(super) fn submit_agents_team_request(&mut self, request: TeamRequest) {
        let operation = if let TeamRequest::Board {
            request:
                codex_app_server_protocol::TeamBoardParams::SubscriptionSet {
                    target,
                    identity,
                    mode,
                },
        } = request
        {
            let Some(operation) = self
                .agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .begin_subscription(
                    target,
                    identity.unwrap_or(codex_app_server_protocol::TeamBoardIdentity::User),
                    mode,
                )
            else {
                return;
            };
            operation
        } else {
            codex_team::TeamOperation::new(request)
        };
        let id = operation.operation_id.clone();
        self.agents_hosts
            .team_operations
            .insert(id.clone(), operation);
        self.retry_agents_team_request(id);
    }

    pub(super) fn retry_agents_team_request(&mut self, operation_id: String) {
        if let Some(pending) = self
            .agents_hosts
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_reads
            .get_mut(&operation_id)
        {
            pending.failed = false;
        }
        let Some(client) = self.agents_hosts.client.clone() else {
            self.agents_hosts
                .board
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .operation_finished(&operation_id, false);
            self.agents_hosts
                .failed_team_operations
                .insert(operation_id);
            self.agents_hosts.notice = Some("The shared team coordinator is unavailable".into());
            return;
        };
        let Some(operation) = self
            .agents_hosts
            .team_operations
            .get(&operation_id)
            .cloned()
        else {
            return;
        };
        self.agents_hosts
            .failed_team_operations
            .remove(&operation_id);
        let sender = self.app_event_tx.unscoped();
        let generations = self.agents_hosts.team_read_generation.clone();
        self.agents_hosts.spawn(async move {
            let result = match client.request_operation(&operation).await {
                Ok(response) => {
                    if let codex_team::TeamResponse::Board {
                        response: codex_app_server_protocol::TeamBoardResponse::Mutation {
                            revision, state: codex_app_server_protocol::TeamBoardMutationState::Applied, ..
                        }
                    } = response {
                        sender.send(AppEvent::AgentsBoardSubscriptionAccepted { operation_id: operation.operation_id.clone(), revision });
                    }
                    let generation =
                        generations.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Ok(snapshot) = client.snapshot().await {
                        sender.send(AppEvent::AgentsHostUpdate(Box::new(
                            HostUpdate::TeamSnapshot {
                                generation,
                                snapshot,
                            },
                        )));
                    }
                    Ok(())
                }
                Err(error) => {
                    if let Some(rejected) = error.downcast_ref::<codex_team::TeamOperationRejected>() {
                        let reason = rejected.reason.clone();
                        let generation = generations.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if let Ok(snapshot) = client.snapshot().await {
                            sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::TeamSnapshot { generation, snapshot })));
                        }
                        Err(TeamOperationFailure::Rejected(reason))
                    } else {
                        Err(TeamOperationFailure::Unknown("Team action outcome is unknown · F9 offers a retry with the same operation ID".into()))
                    }
                },
            };
            sender.send(AppEvent::AgentsHostUpdate(Box::new(
                HostUpdate::TeamOperationFinished {
                    operation_id,
                    result,
                },
            )));
        });
    }
}

#[cfg(test)]
#[path = "agents_hosts_team_tests.rs"]
mod tests;
