//! Operator dashboard. Attachments use a separate child TUI through the relay.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamExecutionMode;
use codex_app_server_protocol::TeamMessageKind;
use codex_team::TeamClient;
use codex_team::TeamRequest;
use codex_team::TeamResponse;
use codex_team::TeamSnapshot;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::TerminalRestoreGuard;
use crate::tui;
use crate::tui::Tui;
use crate::tui::TuiEvent;

mod render;
#[cfg(test)]
#[path = "team_overview/tests.rs"]
mod tests;

#[derive(Clone, Copy, Default)]
enum Grouping {
    #[default]
    Status,
    Project,
    Model,
}

#[derive(Default)]
struct Panel {
    snapshot: Option<TeamSnapshot>,
    selected: usize,
    grouping: Grouping,
    notice: String,
    connected: bool,
    models: Vec<(String, String)>,
    model_target: Option<TeamAddress>,
    model_selected: usize,
    message: Option<String>,
    compose: Option<Compose>,
    board: bool,
    board_selected: usize,
}

enum Compose {
    Request(TeamAddress),
    Rename(TeamAddress),
    Topic,
    Post(String),
    Reply(String),
    Subscribe(TeamAddress),
}

enum Update {
    Snapshot(TeamSnapshot),
    Response(TeamResponse),
    Error(String),
    Offline(String),
}

pub async fn run_team_agents(manifest: &Path, no_alt_screen: bool) -> Result<()> {
    let client = TeamClient::from_manifest(manifest).await?;
    let initialized = tui::init()?;
    let _restore = TerminalRestoreGuard::new();
    let mut tui = Tui::new(
        initialized.terminal,
        initialized.enhanced_keys_supported,
        initialized.stderr_guard,
    );
    tui.set_alt_screen_enabled(!no_alt_screen);
    tui.enter_alt_screen()?;
    let mut input = tui.event_stream();
    let (sender, mut updates) = mpsc::unbounded_channel();
    let watcher = tokio::spawn(watch(client.clone(), sender.clone()));
    let mut panel = Panel {
        notice: "Connecting to coordinator…".into(),
        ..Default::default()
    };
    let result = async {
        loop {
            let height = tui.terminal.last_known_screen_size.height;
            tui.draw(height, |frame| panel.render(frame.area(), frame.buffer_mut()))?;
            tokio::select! {
                Some(update) = updates.recv() => panel.update(update),
                event = input.next() => match event {
                    Some(TuiEvent::Key(key)) if key.kind != KeyEventKind::Release => {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { break; }
                        let control = match key.code {
                            KeyCode::F(6) => Some(TeamExecutionMode::Paused),
                            KeyCode::F(7) => Some(TeamExecutionMode::Running),
                            KeyCode::F(8) => Some(TeamExecutionMode::Stopped),
                            _ => None,
                        };
                        if let Some(mode) = control {
                            submit(&client, &sender, TeamRequest::Control { mode });
                            continue;
                        }
                        if let Some(message) = panel.message.as_mut() {
                            match key.code {
                                KeyCode::Esc => { panel.message = None; panel.compose = None; },
                                KeyCode::Backspace => { message.pop(); },
                                KeyCode::Char(character) => message.push(character),
                                KeyCode::Enter => {
                                    let body = panel.message.take().unwrap_or_default();
                                    if let Some(compose) = panel.compose.take() && !body.trim().is_empty() {
                                        let request = match compose {
                                            Compose::Request(address) => Some(TeamRequest::Send { recipient: address, body, kind: TeamMessageKind::Request, reply_to: None, delivery: TeamDeliveryMode::NextTurn }),
                                            Compose::Rename(address) => Some(TeamRequest::Enroll { address, alias: body }),
                                            Compose::Post(topic) => Some(TeamRequest::Post { topic, body }),
                                            Compose::Reply(message_id) => Some(TeamRequest::Reply { message_id, body }),
                                            Compose::Subscribe(address) => Some(TeamRequest::Subscribe { address, topic: body, enabled: true }),
                                            Compose::Topic => { panel.compose = Some(Compose::Post(body)); panel.message = Some(String::new()); None },
                                        };
                                        if let Some(request) = request { submit(&client, &sender, request); }
                                    }
                                }
                                _ => {},
                            }
                            continue;
                        }
                        if panel.model_target.is_some() {
                            match key.code {
                                KeyCode::Esc => { panel.model_target = None; panel.models.clear(); },
                                KeyCode::Up => panel.model_selected = panel.model_selected.saturating_sub(1),
                                KeyCode::Down => panel.model_selected = (panel.model_selected + 1).min(panel.models.len().saturating_sub(1)),
                                KeyCode::Enter => if let Some((model, effort)) = panel.models.get(panel.model_selected).cloned()
                                    && let Some(address) = panel.model_target.take() {

                                    panel.models.clear();
                                    submit(&client, &sender, TeamRequest::SetModel { address, model, effort: Some(effort) });
                                },
                                _ => {},
                            }
                            continue;
                        }
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Up if panel.board => panel.board_selected = panel.board_selected.saturating_sub(1),
                            KeyCode::Down if panel.board => panel.board_selected = (panel.board_selected + 1).min(panel.snapshot.as_ref().map_or(0, |snapshot| snapshot.messages.len().saturating_sub(1))),
                            KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
                            KeyCode::Down => panel.selected = (panel.selected + 1).min(panel.ordered_agents().len().saturating_sub(1)),
                            KeyCode::Char('g') => { panel.grouping = match panel.grouping { Grouping::Status => Grouping::Project, Grouping::Project => Grouping::Model, Grouping::Model => Grouping::Status }; panel.selected = 0; },
                            KeyCode::Char('b') => panel.board = !panel.board,
                            KeyCode::Char('p') => submit(&client, &sender, TeamRequest::Control { mode: TeamExecutionMode::Paused }),
                            KeyCode::Char('r') => submit(&client, &sender, TeamRequest::Control { mode: TeamExecutionMode::Running }),
                            KeyCode::Char('s') => submit(&client, &sender, TeamRequest::Control { mode: TeamExecutionMode::Stopped }),
                            KeyCode::Char('n' | 'e' | 'l') if panel.connected => if let Some(address) = panel.selected_address() {
                                panel.compose = Some(match key.code { KeyCode::Char('e') => Compose::Rename(address), KeyCode::Char('l') => Compose::Subscribe(address), _ => Compose::Request(address) });
                                panel.message = Some(String::new());
                            },
                            KeyCode::Char('t') if panel.connected => { panel.compose = Some(Compose::Topic); panel.message = Some(String::new()); },
                            KeyCode::Char('f') if panel.connected && panel.board => if let Some(message) = panel.snapshot.as_ref().and_then(|snapshot| snapshot.messages.iter().rev().nth(panel.board_selected)) {
                                panel.compose = Some(Compose::Reply(message.message.id.clone()));
                                panel.message = Some(String::new());
                            },
                            KeyCode::Char('m') if panel.connected => if let Some(address) = panel.selected_address() {
                                panel.model_target = Some(address.clone());
                                panel.model_selected = 0;
                                panel.notice = "Loading models from the owning host…".into();
                                submit(&client, &sender, TeamRequest::Models { machine_id: address.machine_id, server_id: address.server_id });
                            },
                            KeyCode::Enter if panel.connected => if let Some(address) = panel.selected_address() {
                                let mut command = client.attach_command(&address)?;
                                match tui.with_restored(|| command.status()).await {
                                    Ok(status) if status.success() => panel.notice = "Returned from agent session".into(),
                                    Ok(status) => panel.notice = format!("Agent terminal exited: {status}"),
                                    Err(error) => panel.notice = error.to_string(),
                                }
                                submit(&client, &sender, TeamRequest::Snapshot);
                            },
                            _ => {},
                        }
                    }
                    Some(TuiEvent::Paste(value)) => if let Some(message) = panel.message.as_mut() { message.push_str(&value); },
                    None => break,
                    _ => {},
                },
            }
        }
        Ok(())
    }.await;
    watcher.abort();
    tui.leave_alt_screen()?;
    result
}

fn submit(client: &TeamClient, sender: &mpsc::UnboundedSender<Update>, request: TeamRequest) {
    let client = client.clone();
    let sender = sender.clone();
    tokio::spawn(async move {
        match client.request(request).await {
            Ok(response) => {
                let _ = sender.send(Update::Response(response));
            }
            Err(error) => {
                let _ = sender.send(Update::Error(error.to_string()));
            }
        }
    });
}

async fn watch(client: TeamClient, sender: mpsc::UnboundedSender<Update>) {
    let mut delay = 1;
    loop {
        let result = async {
            let mut events = client.events().await?;
            loop {
                events.changed().await?;
                let snapshot = client.snapshot().await?;
                if sender.send(Update::Snapshot(snapshot)).is_err() {
                    return Ok::<(), anyhow::Error>(());
                }
                delay = 1;
            }
        }
        .await;
        match result {
            Ok(()) => return,
            Err(error) => {
                if sender.send(Update::Offline(error.to_string())).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

impl Panel {
    fn update(&mut self, update: Update) {
        match update {
            Update::Snapshot(snapshot) | Update::Response(TeamResponse::Snapshot { snapshot }) => {
                let selected = self.selected_address();
                self.snapshot = Some(snapshot);
                self.selected = self
                    .ordered_agents()
                    .iter()
                    .position(|agent| Some(&agent.enrollment.address) == selected.as_ref())
                    .unwrap_or(0);
                if !self.connected {
                    self.notice.clear();
                }
                self.connected = true;
            }
            Update::Response(TeamResponse::Models { data }) => {
                self.models = data
                    .into_iter()
                    .flat_map(
                        |Model {
                             model,
                             supported_reasoning_efforts,
                             default_reasoning_effort,
                             ..
                         }| {
                            if supported_reasoning_efforts.is_empty() {
                                return vec![(model, default_reasoning_effort.to_string())];
                            }
                            supported_reasoning_efforts
                                .into_iter()
                                .map(move |effort| {
                                    (model.clone(), effort.reasoning_effort.to_string())
                                })
                                .collect()
                        },
                    )
                    .collect();
                self.notice = "Select a model and effort for subsequent turns".into();
            }
            Update::Response(_) => self.notice = "Saved; waiting for host acknowledgement".into(),
            Update::Error(error) => {
                self.notice = error;
                self.model_target = None;
                self.models.clear();
            }
            Update::Offline(error) => {
                self.connected = false;
                self.notice = format!("Coordinator disconnected: {error}");
            }
        }
    }

    fn ordered_agents(&self) -> Vec<&codex_team::Agent> {
        let mut agents = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.agents.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        agents.sort_by_key(|agent| {
            (
                &agent.enrollment.address.machine_id,
                &agent.enrollment.address.server_id,
                self.group(agent),
                &agent.enrollment.alias,
            )
        });
        agents
    }

    fn group<'a>(&self, agent: &'a codex_team::Agent) -> &'a str {
        match self.grouping {
            Grouping::Status => {
                if agent.enrollment.manually_held {
                    "manual hold"
                } else {
                    &agent.status
                }
            }
            Grouping::Project => agent.project.as_deref().unwrap_or("No project"),
            Grouping::Model => agent.next_model.as_deref().unwrap_or("Unknown model"),
        }
    }

    fn selected_address(&self) -> Option<TeamAddress> {
        self.ordered_agents()
            .get(self.selected)
            .map(|agent| agent.enrollment.address.clone())
    }
}
