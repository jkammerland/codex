//! Delivery receipts and cancellation uncertainty remain inspectable in the existing popup stack.

use super::agents_hosts_team::TeamCompose;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::render::renderable::Renderable;
use crate::wrapping::word_wrap_lines;
use codex_team::TeamRequest;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
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
        let Some(record) = self.agents_hosts.snapshot.as_ref().and_then(|snapshot| {
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
        let mut lines = vec![
            format!(
                "{destination} · {:?} · {:?} · {}",
                record.message.kind,
                record.delivery,
                if record.published {
                    "published"
                } else {
                    "queued"
                }
            )
            .bold()
            .into(),
            record.message.body.clone().into(),
        ];
        if let Some(url) = &record.external_url {
            lines.push(url.clone().cyan().into());
        }
        if let Some(error) = &record.error {
            lines.push(error.clone().red().into());
        }
        for receipt in record.receipts.values() {
            lines.push(format!("{}: {:?}", receipt.thread_id, receipt.state).into());
            if let Some(detail) = &receipt.detail {
                lines.push(detail.clone().into());
            }
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
        items.push(SelectionItem {
            name: "Retry delivery".into(),
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
        self.chat_widget.show_selection_view(SelectionViewParams {
            header: Box::new(MessageHeader(lines)),
            items,
            ..SelectionViewParams::picker()
        });
    }
}
