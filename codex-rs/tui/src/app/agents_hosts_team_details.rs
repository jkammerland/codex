//! Delivery receipts and cancellation uncertainty remain inspectable in the existing popup stack.

use super::agents_hosts::ThreadTarget;
use super::agents_hosts_team::TeamCompose;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::render::renderable::Renderable;
use crate::wrapping::word_wrap_lines;
use codex_app_server_protocol::*;
use codex_team::TeamRequest;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

struct MessageHeader(Vec<Line<'static>>);

impl Renderable for MessageHeader {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        Renderable::render(
            &Paragraph::new(word_wrap_lines(&self.0, usize::from(area.width))),
            area,
            buffer,
        );
    }
    fn desired_height(&self, width: u16) -> u16 {
        word_wrap_lines(&self.0, usize::from(width)).len() as u16
    }
}

impl App {
    pub(super) fn open_agents_team_explanations(&mut self, mut target: ThreadTarget) {
        target.host = self.agents_hosts.canonical_host(&target.host);
        let Some(snapshot) = &self.agents_hosts.snapshot else {
            return;
        };
        let mut items = Vec::new();
        for action in [
            TeamExplainableAction::Enroll,
            TeamExplainableAction::QueueMessage,
            TeamExplainableAction::ProcessMessages,
            TeamExplainableAction::Post,
            TeamExplainableAction::SelectModel,
            TeamExplainableAction::Subscribe,
            TeamExplainableAction::Resume,
            TeamExplainableAction::Pause,
            TeamExplainableAction::Stop,
            TeamExplainableAction::Leave,
        ] {
            let explanation = codex_team::explain_action(
                snapshot,
                TeamExplainActionParams {
                    action,
                    target: Some(target.team_address()),
                },
            );
            items.push(SelectionItem {
                name: format!("{} · {}", action.label(), explanation.availability.label()),
                description: Some(format!(
                    "{} · {} · {}{}",
                    explanation.reason,
                    explanation.owner,
                    explanation.freshness.label(),
                    explanation
                        .resolution
                        .map_or(String::new(), |resolution| format!(" · {resolution}"))
                )),
                ..Default::default()
            });
        }
        let ids = if target.host == self.agents_hosts.foreground {
            self.pending_app_server_requests
                .owned_approval_ids(&target.thread_id.to_string())
        } else {
            self.agents_hosts
                .hosts
                .get(&target.host)
                .map(|host| {
                    host.state
                        .pending_app_server_requests
                        .owned_approval_ids(&target.thread_id.to_string())
                })
                .unwrap_or_default()
        };
        if let Some(host) = self
            .agents_hosts
            .hosts
            .get(&target.host)
            .filter(|host| host.connected)
        {
            for request_id in ids {
                let target = target.clone();
                let epoch = host.epoch;
                items.push(SelectionItem {
                    name: "Requires authorization · open pending approval".into(),
                    description: Some(format!("Request {request_id:?} · owning worker · inspect the requested scope and available decisions in its original prompt")),
                    actions: vec![Box::new(move |sender| sender.unscoped().send(AppEvent::AgentsTeamOpenApproval { target: target.clone(), request_id: request_id.clone(), epoch }))],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
        }
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Action availability and restrictions".into()),
            subtitle: Some(
                "Observations are rechecked on execution. Team controls grant no tool permissions."
                    .into(),
            ),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn open_agents_team_pending_approval(
        &mut self,
        target: ThreadTarget,
        request_id: RequestId,
        epoch: u64,
    ) {
        let Some(host) = self
            .agents_hosts
            .hosts
            .get(&target.host)
            .filter(|host| host.connected && host.epoch == epoch)
        else {
            return;
        };
        let ledger = if target.host == self.agents_hosts.foreground {
            &self.pending_app_server_requests
        } else {
            &host.state.pending_app_server_requests
        };
        if !ledger
            .owned_approval_ids(&target.thread_id.to_string())
            .contains(&request_id)
        {
            return;
        }
        // Navigation replays only still-pending prompts through their original
        // reply handles. This action never answers or transfers an approval.
        self.app_event_tx.send(AppEvent::AgentsHostAction {
            event: Box::new(AppEvent::SelectAgentsOverviewThread {
                thread_id: target.thread_id,
            }),
            target,
        });
    }

    pub(super) fn open_agents_team_deliveries(&mut self) {
        let mut items = Vec::new();
        if let Some(snapshot) = &self.agents_hosts.snapshot {
            for record in snapshot.messages.iter().rev() {
                let id = record.message.id.clone();
                items.push(SelectionItem {
                    name: record
                        .message
                        .body
                        .lines()
                        .next()
                        .unwrap_or("Message")
                        .into(),
                    description: Some(format!(
                        "{:?} · {} receipts{}",
                        record.message.kind,
                        record.receipts.len(),
                        record
                            .error
                            .as_ref()
                            .map_or(String::new(), |error| format!(" · {error}"))
                    )),
                    actions: vec![Box::new(move |sender| {
                        sender
                            .unscoped()
                            .send(AppEvent::AgentsTeamMessage(id.clone()))
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                });
            }
            for rejected in &snapshot.rejected_outbox {
                items.push(SelectionItem {
                    name: format!("Rejected outgoing entry {}", rejected.entry.id),
                    description: Some(rejected.reason.clone()),
                    ..Default::default()
                });
            }
        }
        for id in &self.agents_hosts.failed_team_operations {
            let id = id.clone();
            items.push(SelectionItem {
                name: "Retry action with unknown outcome".into(),
                description: Some(id.clone()),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamRetry(id.clone()))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Messages and delivery diagnostics".into()),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn open_agents_team_message(&mut self, id: String) {
        let Some(snapshot) = self.agents_hosts.snapshot.as_ref() else {
            return;
        };
        let Some(record) = Some(snapshot).and_then(|snapshot| {
            snapshot
                .messages
                .iter()
                .find(|record| record.message.id == id)
        }) else {
            return;
        };
        let destination = record
            .message
            .topic
            .clone()
            .or_else(|| {
                record
                    .message
                    .recipient
                    .as_ref()
                    .map(|address| address.thread_id.clone())
            })
            .unwrap_or_default();
        let status = codex_team::message_statuses(snapshot)
            .into_iter()
            .find(|status| status.message_id == id);
        let mut lines = vec![
            format!(
                "{destination} · {:?} · {:?} · {}",
                record.message.kind,
                record.delivery,
                status
                    .as_ref()
                    .map_or("Intake accepted at coordinator", |status| status
                        .intake
                        .label())
            )
            .bold()
            .into(),
        ];

        // Preserve authored line and paragraph breaks before wrapping the body.
        lines.extend(Text::from(record.message.body.clone()).lines);

        if let Some(url) = &record.external_url {
            lines.push(url.clone().cyan().into());
        }
        if let Some(error) = &record.error {
            lines.push(error.clone().red().into());
        }
        if let Some(status) = &status {
            lines.extend(message_status_lines(status));
        }
        let mut items = Vec::new();
        for (name, delivery) in [
            (
                "Reply · next turn (preferred)",
                codex_app_server_protocol::TeamDeliveryMode::NextTurn,
            ),
            (
                "Reply · safe boundary",
                codex_app_server_protocol::TeamDeliveryMode::SafeBoundary,
            ),
        ] {
            let reply_id = id.clone();
            items.push(SelectionItem {
                name: name.into(),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamCompose(TeamCompose::Reply(
                            reply_id.clone(),
                            delivery,
                        )))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        if record.error.is_some() {
            items.push(SelectionItem {
                name: "Retry external publication".into(),
                description: Some(
                    "Retries failed publication; it does not replay an admitted turn".into(),
                ),
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamRequest(TeamRequest::Retry {
                            message_id: id.clone(),
                        }))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        self.chat_widget.show_selection_view(SelectionViewParams {
            header: Box::new(MessageHeader(lines)),
            items,
            ..SelectionViewParams::picker()
        });
    }
}

fn message_status_lines(status: &TeamMessageStatus) -> Vec<Line<'static>> {
    let mut lines =
        vec![format!("From {} · {}", status.sender_name, status.freshness.label()).into()];
    for recipient in &status.recipients {
        lines.push(
            format!(
                "{}: {} · {}",
                recipient.name,
                if recipient.abandonment.is_some() {
                    "Processing abandoned"
                } else {
                    recipient.progress.label()
                },
                recipient.freshness.label()
            )
            .into(),
        );
        if let Some(abandonment) = &recipient.abandonment {
            lines.push(
                format!("Operator reason: {}", abandonment.reason)
                    .magenta()
                    .into(),
            );
        }
        if let Some(receipt) = &recipient.receipt {
            if let Some(turn_id) = &receipt.turn_id {
                lines.push(format!("Turn {turn_id}").dim().into());
            }
            if let Some(detail) = &receipt.detail {
                lines.push(detail.clone().into());
            }
        }
    }
    lines.push(
        format!(
            "{} related replies · replying is optional",
            status.related_replies.len()
        )
        .into(),
    );
    lines.push(
        "Admitted input may not have reached the model. Turn ended does not mean task fulfilled."
            .dim()
            .into(),
    );
    lines
}

#[cfg(test)]
#[path = "agents_hosts_team_details_tests.rs"]
mod tests;
