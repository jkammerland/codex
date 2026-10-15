use codex_app_server_protocol::TeamExecutionMode;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::ListState;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidget;
use ratatui::widgets::Widget;

use super::Grouping;
use super::Panel;

impl Panel {
    pub(super) fn render(&self, area: Rect, buffer: &mut Buffer) {
        let [header, content, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .areas(area);
        let Some(snapshot) = &self.snapshot else {
            Paragraph::new(self.notice.as_str())
                .block(Block::bordered().title("Codex team"))
                .render(area, buffer);
            return;
        };
        let unconfirmed = snapshot
            .servers
            .iter()
            .filter(|server| {
                !server.connected || server.applied_revision != Some(snapshot.control.revision)
            })
            .count();
        let mode = match snapshot.control.mode {
            TeamExecutionMode::Running => "Running",
            TeamExecutionMode::Paused => "Paused",
            TeamExecutionMode::Stopped => "Stopped",
        };
        let title = if self.connected {
            format!(
                "Codex team · {mode} · revision {} · {unconfirmed} host(s) unconfirmed",
                snapshot.control.revision
            )
        } else {
            format!("Codex team · disconnected · last known: {mode}")
        };
        let description = match snapshot.control.mode {
            TeamExecutionMode::Running => {
                "User settings apply to subsequent turns. Manual holds and approvals stay in place."
            }
            TeamExecutionMode::Paused => {
                "New execution is held. Already admitted model requests and tools may finish."
            }
            TeamExecutionMode::Stopped => {
                "Turns interrupted; tracked work cancellation must be confirmed by each host."
            }
        };
        let mut heading = vec![Line::from(title.bold())];
        heading.extend(
            textwrap::wrap(description, usize::from(header.width).max(1))
                .into_iter()
                .map(|line| Line::from(line.into_owned().dim())),
        );
        Paragraph::new(heading).render(header, buffer);

        if self.model_target.is_some() {
            let items = self
                .models
                .iter()
                .map(|(model, effort)| {
                    ListItem::new(format!(
                        "{model} · {effort}{}",
                        if effort == "ultra" {
                            " — explicit Ultra selection"
                        } else {
                            ""
                        }
                    ))
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(self.model_selected));
            StatefulWidget::render(
                List::new(items)
                    .block(
                        Block::bordered()
                            .title("Next-turn model and effort · Enter selects · Esc cancels"),
                    )
                    .highlight_symbol("› ")
                    .highlight_style(ratatui::style::Style::new().reversed()),
                content,
                buffer,
                &mut state,
            );
        } else if self.board {
            let mut items = Vec::new();
            for record in snapshot.messages.iter().rev() {
                let mut lines = Vec::new();
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
                lines.push(Line::from(
                    format!(
                        "{destination} · {:?} · {}",
                        record.message.kind,
                        if record.published {
                            "published"
                        } else {
                            "queued"
                        }
                    )
                    .cyan(),
                ));
                lines.extend(
                    textwrap::wrap(
                        &record.message.body,
                        usize::from(content.width.saturating_sub(2)).max(1),
                    )
                    .into_iter()
                    .map(|line| Line::from(line.into_owned())),
                );
                if let Some(error) = &record.error {
                    lines.push(Line::from(error.clone().red()));
                }
                if let Some(url) = &record.external_url {
                    lines.push(Line::from(url.clone().cyan()));
                }
                for receipt in record.receipts.values() {
                    lines.push(Line::from(
                        format!("  {}: {:?}", receipt.thread_id, receipt.state).dim(),
                    ));
                    if let Some(detail) = &receipt.detail {
                        lines.push(Line::from(format!("    {detail}").cyan()));
                    }
                }
                lines.push(Line::default());
                items.push(ListItem::new(lines));
            }
            for rejected in &snapshot.rejected_outbox {
                items.push(ListItem::new(Line::from(
                    format!(
                        "Rejected outgoing entry {}/{}/{}: {}",
                        rejected.machine_id,
                        rejected.server_id,
                        rejected.entry.sequence,
                        rejected.reason
                    )
                    .red(),
                )));
            }
            let mut state = ListState::default().with_selected(Some(self.board_selected));
            StatefulWidget::render(
                List::new(items)
                    .block(Block::bordered().title("Messages · ↑↓ select · f reply · t post"))
                    .highlight_symbol("› ")
                    .highlight_style(ratatui::style::Style::new().reversed()),
                content,
                buffer,
                &mut state,
            );
        } else {
            let agents = self.ordered_agents();
            let selected = agents
                .get(self.selected)
                .map(|agent| &agent.enrollment.address);
            let mut rows = Vec::new();
            let mut selected_row = None;
            let mut previous_machine = None;
            for server in &snapshot.servers {
                if previous_machine != Some(&server.machine_id) {
                    rows.push(ListItem::new(Line::from(
                        format!("{} ({})", server.machine_label, server.machine_id).bold(),
                    )));
                    previous_machine = Some(&server.machine_id);
                }
                let connection = if server.connected {
                    "connected"
                } else {
                    "disconnected"
                };
                let acknowledged = server.applied_revision.map_or_else(
                    || "unconfirmed".into(),
                    |revision| format!("applied {revision}"),
                );
                rows.push(ListItem::new(Line::from(
                    format!("  {} · {connection} · {acknowledged}", server.server_label).dim(),
                )));
                if let Some(error) = &server.error {
                    rows.push(ListItem::new(Line::from(format!("    {error}").cyan())));
                }
                let mut group = None;
                for agent in agents.iter().filter(|agent| {
                    agent.enrollment.address.machine_id == server.machine_id
                        && agent.enrollment.address.server_id == server.server_id
                }) {
                    let next_group = self.group(agent);
                    if group != Some(next_group) {
                        rows.push(ListItem::new(Line::from(
                            format!("    {next_group}").cyan(),
                        )));
                        group = Some(next_group);
                    }
                    if selected == Some(&agent.enrollment.address) {
                        selected_row = Some(rows.len());
                    }
                    let next = format!(
                        "{} / {}",
                        agent.next_model.as_deref().unwrap_or("unknown"),
                        agent.next_effort.as_deref().unwrap_or("default")
                    );
                    let current = agent
                        .current_model
                        .as_ref()
                        .map(|model| {
                            format!(
                                "{model} / {}",
                                agent.current_effort.as_deref().unwrap_or("default")
                            )
                        })
                        .unwrap_or_else(|| {
                            if agent.status == "working" {
                                "unknown".into()
                            } else {
                                "—".into()
                            }
                        });
                    let held = if agent.enrollment.manually_held {
                        " · manual hold"
                    } else {
                        ""
                    };
                    let continuation = if agent.enrollment.continuation_turn_id.is_some() {
                        " · continuation pending"
                    } else {
                        ""
                    };
                    let flight = agent
                        .runtime
                        .as_ref()
                        .filter(|runtime| {
                            runtime.thread_id == runtime.root_thread_id && runtime.in_flight > 0
                        })
                        .map(|runtime| {
                            format!(" · {} family operation(s) admitted", runtime.in_flight)
                        })
                        .unwrap_or_default();
                    rows.push(ListItem::new(vec![
                        Line::from(format!(
                            "      {}{held}{continuation}{flight}",
                            agent.enrollment.alias
                        )),
                        Line::from(format!("        current: {current}   next: {next}").dim()),
                    ]));
                    if let Some(runtime) = &agent.runtime {
                        if let Some(error) = &runtime.model_error {
                            rows.push(ListItem::new(Line::from(format!("        {error}").cyan())));
                        }
                        for job in &runtime.jobs {
                            rows.push(ListItem::new(Line::from(
                                format!(
                                    "        {}: {:?}{}",
                                    job.tool,
                                    job.status,
                                    job.detail
                                        .as_ref()
                                        .map(|detail| format!(" · {detail}"))
                                        .unwrap_or_default()
                                )
                                .cyan(),
                            )));
                        }
                    }
                }
            }
            if agents.is_empty() {
                rows.push(ListItem::new("Enroll existing sessions with: codex team --manifest FILE enroll machine/server/thread ALIAS"));
            }
            let grouping = match self.grouping {
                Grouping::Status => "status",
                Grouping::Project => "project",
                Grouping::Model => "model",
            };
            let mut state = ListState::default().with_selected(selected_row);
            StatefulWidget::render(
                List::new(rows)
                    .block(Block::bordered().title(format!("Machines → servers → {grouping}")))
                    .highlight_symbol("› ")
                    .highlight_style(ratatui::style::Style::new().reversed()),
                content,
                buffer,
                &mut state,
            );
        }

        let lines = if let Some(message) = &self.message {
            let prompt = match &self.compose {
                Some(super::Compose::Request(_)) => "Request to selected agent",
                Some(super::Compose::Rename(_)) => "New team alias",
                Some(super::Compose::Topic) => "Board topic",
                Some(super::Compose::Post(_)) => "Board message",
                Some(super::Compose::Reply(_)) => "Reply to selected message",
                Some(super::Compose::Subscribe(_)) => "Board topic to listen to",
                None => "Message",
            };
            vec![
                Line::from(format!("{prompt} · Enter submits · Esc cancels").bold()),
                Line::from(format!("> {message}")),
                Line::from("F6 pause all · F7 resume all · F8 stop all".dim()),
            ]
        } else if self.model_target.is_some() {
            vec![
                Line::from(self.notice.clone().cyan()),
                Line::from("↑↓ choose · Enter selects for next turn · Esc cancels".dim()),
                Line::from("F6 pause all · F7 resume all · F8 stop all".dim()),
            ]
        } else {
            vec![
                Line::from(self.notice.clone().cyan()),
                Line::from(
                    "↑↓ select · Enter open · g group · m model · n request · b board · e rename · l listen".dim(),
                ),
                Line::from("p/F6 pause all · r/F7 resume all · s/F8 stop all · q leave panel".dim()),
            ]
        };
        Paragraph::new(lines).render(footer, buffer);
    }
}
