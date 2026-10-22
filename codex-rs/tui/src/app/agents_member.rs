//! Operator creation keeps the target host and draft explicit until final submission.

use super::agents_hosts::HostKey;
use super::agents_hosts::ThreadTarget;
use super::agents_hosts_team::TeamCompose;
use super::agents_operator_operations::OperationHeader;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::SessionSource;
use codex_team::TeamMemberCreateRequest;
use codex_team::TeamRequest;
use codex_team::TeamResponse;
use ratatui::style::Stylize;

const CATALOG_VIEW: &str = "agents-member-catalog";

#[derive(Clone, Debug)]
pub(crate) struct MemberDraft {
    pub(crate) request: TeamMemberCreateRequest,
    pub(crate) host_label: String,
}

#[derive(Debug)]
pub(crate) enum MemberAction {
    Open,
    Host(HostKey),
    Checkout(MemberDraft),
    LoadModels(MemberDraft),
    ModelsLoaded {
        request_id: Uuid,
        draft: MemberDraft,
        result: Result<Vec<Model>, String>,
    },
    CancelCatalog(Uuid),
    Model {
        draft: MemberDraft,
        model: Box<Model>,
    },
    Confirm(MemberDraft),
    Submit(MemberDraft),
    EnrollExisting,
}

impl App {
    pub(super) fn agents_member_menu_items(&self) -> Vec<SelectionItem> {
        vec![
            SelectionItem {
                name: "Create member".into(),
                description: Some("Choose a machine, checkout, alias, model and effort".into()),
                actions: vec![Box::new(|sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsMember(Box::new(MemberAction::Open)));
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Enroll existing session".into(),
                description: Some("Keep its history and settings; add a team alias".into()),
                actions: vec![Box::new(|sender| {
                    sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                        MemberAction::EnrollExisting,
                    )));
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Team operations".into(),
                description: Some("Member creation and Board cleanup progress".into()),
                actions: vec![Box::new(|sender| {
                    sender.unscoped().send(AppEvent::AgentsOperatorOperations)
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ]
    }

    pub(super) fn open_agents_team_general_menu(&mut self) {
        let mut items = self.agents_member_menu_items();
        items.push(SelectionItem {
            name: "Machines and connections".into(),
            actions: vec![Box::new(|sender| {
                sender.unscoped().send(AppEvent::AgentsMachinesMenu)
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Shared team".into()),
            subtitle: Some("F6 Pause · F7 Resume · F8 Stop".into()),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn handle_agents_member(&mut self, action: MemberAction) {
        match action {
            MemberAction::Open => {
                self.agents_hosts.member_catalog_request = None;
                let items: Vec<_> = self
                    .agents_hosts
                    .snapshot
                    .as_ref()
                    .map(|snapshot| {
                        snapshot
                            .servers
                            .iter()
                            .map(|server| {
                                let host = HostKey {
                                    machine_id: server.machine_id.clone(),
                                    server_id: server.server_id.clone(),
                                };
                                SelectionItem {
                                    name: format!(
                                        "{} / {}",
                                        server.machine_label, server.server_label
                                    ),
                                    description: Some(if server.connected {
                                        "Create an idle member on this machine".into()
                                    } else {
                                        "Unavailable · reconnect this machine first".into()
                                    }),
                                    is_disabled: !server.connected,
                                    disabled_reason: (!server.connected)
                                        .then(|| "Machine is unavailable".into()),
                                    actions: vec![Box::new(move |sender| {
                                        sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                                            MemberAction::Host(host.clone()),
                                        )))
                                    })],
                                    dismiss_on_select: true,
                                    ..Default::default()
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let subtitle = if items.is_empty() {
                    "No machines are available yet. Open Machines and connections to check the configured hosts."
                } else {
                    "The new member joins the existing shared team."
                };
                self.chat_widget.show_selection_view(SelectionViewParams {
                    title: Some("Create member · machine".into()),
                    subtitle: Some(subtitle.into()),
                    items,
                    ..SelectionViewParams::picker()
                });
            }
            MemberAction::Host(host) => {
                let Some(server) = self.agents_hosts.snapshot.as_ref().and_then(|snapshot| {
                    snapshot.servers.iter().find(|server| {
                        server.machine_id == host.machine_id && server.server_id == host.server_id
                    })
                }) else {
                    self.agents_hosts.notice =
                        Some("The selected machine is no longer configured".into());
                    return;
                };
                if !server.connected {
                    self.agents_hosts.notice = Some("The selected machine is unavailable".into());
                    return;
                }
                let draft = MemberDraft {
                    host_label: format!("{} / {}", server.machine_label, server.server_label),
                    request: TeamMemberCreateRequest {
                        machine_id: host.machine_id,
                        server_id: host.server_id,
                        alias: String::new(),
                        cwd: String::new(),
                        model: None,
                        effort: None,
                    },
                };
                self.open_agents_team_compose(TeamCompose::MemberAlias(draft));
            }
            MemberAction::Checkout(draft) => {
                self.open_agents_team_compose(TeamCompose::MemberCheckout(draft))
            }
            MemberAction::LoadModels(draft) => {
                let Some(client) = self.agents_hosts.client.clone() else {
                    self.agents_hosts.notice =
                        Some("The shared team coordinator is unavailable".into());
                    return;
                };
                let request_id = Uuid::new_v4();
                self.agents_hosts.member_catalog_request = Some(request_id);
                self.chat_widget.show_selection_view(SelectionViewParams {
                    view_id: Some(CATALOG_VIEW),
                    title: Some("Loading member models".into()),
                    subtitle: Some(format!(
                        "Fetching choices from {} · Esc cancels",
                        draft.host_label
                    )),
                    on_cancel: Some(Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                            MemberAction::CancelCatalog(request_id),
                        )))
                    })),
                    ..SelectionViewParams::picker()
                });
                let sender = self.app_event_tx.unscoped();
                self.agents_hosts.spawn(async move {
                    let result = match client
                        .request(TeamRequest::Models {
                            machine_id: draft.request.machine_id.clone(),
                            server_id: draft.request.server_id.clone(),
                        })
                        .await
                    {
                        Ok(TeamResponse::Models { data }) => Ok(data),
                        Ok(_) => {
                            Err("The machine returned an unexpected model catalog response".into())
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    sender.send(AppEvent::AgentsMember(Box::new(
                        MemberAction::ModelsLoaded {
                            request_id,
                            draft,
                            result,
                        },
                    )));
                });
            }
            MemberAction::CancelCatalog(id) => {
                if self.agents_hosts.member_catalog_request == Some(id) {
                    self.agents_hosts.member_catalog_request = None;
                }
            }
            MemberAction::ModelsLoaded {
                request_id,
                draft,
                result,
            } => {
                if self.agents_hosts.member_catalog_request != Some(request_id) {
                    return;
                }
                self.agents_hosts.member_catalog_request = None;
                let params = match result {
                    Ok(models) => member_models(draft, models),
                    Err(error) => SelectionViewParams {
                        title: Some("Member models unavailable".into()),
                        subtitle: Some(error),
                        items: vec![SelectionItem {
                            name: "Retry this machine's catalog".into(),
                            actions: vec![Box::new(move |sender| {
                                sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                                    MemberAction::LoadModels(draft.clone()),
                                )))
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        }],
                        ..SelectionViewParams::picker()
                    },
                };
                self.chat_widget
                    .replace_selection_view_if_present(CATALOG_VIEW, params);
            }
            MemberAction::Model { mut draft, model } => {
                draft.request.model = Some(model.model.clone());
                if model.supported_reasoning_efforts.is_empty() {
                    self.handle_agents_member(MemberAction::Confirm(draft));
                    return;
                }
                // An explicit model default must not inherit a different effort from the host config.
                let mut defaults = draft.clone();
                defaults.request.effort = Some(model.default_reasoning_effort.to_string());
                let mut items = vec![SelectionItem {
                    name: "Model default effort".into(),
                    description: Some(model.default_reasoning_effort.to_string()),
                    actions: vec![Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                            MemberAction::Confirm(defaults.clone()),
                        )))
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }];
                items.extend(model.supported_reasoning_efforts.into_iter().map(|effort| {
                    let mut selected = draft.clone();
                    selected.request.effort = Some(effort.reasoning_effort.to_string());
                    SelectionItem {
                        name: effort.reasoning_effort.to_string(),
                        description: Some(effort.description),
                        actions: vec![Box::new(move |sender| {
                            sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                                MemberAction::Confirm(selected.clone()),
                            )))
                        })],
                        dismiss_on_select: true,
                        ..Default::default()
                    }
                }));
                self.chat_widget.show_selection_view(SelectionViewParams {
                    title: Some(format!("Member effort · {}", model.display_name)),
                    items,
                    ..SelectionViewParams::picker()
                });
            }
            MemberAction::Confirm(draft) => self
                .chat_widget
                .show_selection_view(member_confirmation(draft)),
            MemberAction::Submit(draft) => {
                self.agents_hosts.notice = Some(format!(
                    "Creating {} · progress is available in Team operations",
                    draft.request.alias
                ));
                self.submit_agents_team_request(TeamRequest::CreateMember {
                    request: draft.request,
                });
            }
            MemberAction::EnrollExisting => self.open_agents_member_enrollment(),
        }
    }

    fn open_agents_member_enrollment(&mut self) {
        let mut candidates = Vec::new();
        for (host, state) in &self.agents_hosts.hosts {
            let threads = if *host == self.agents_hosts.foreground {
                &self.agents_overview.threads
            } else {
                &state.state.agents_overview.threads
            };
            for (thread_id, thread) in threads {
                let Some(thread) = thread else { continue };
                let target = ThreadTarget {
                    host: host.clone(),
                    thread_id: *thread_id,
                };
                if thread.ephemeral
                    || matches!(thread.source, SessionSource::SubAgent(_))
                    || self.agents_hosts.snapshot.as_ref().is_some_and(|snapshot| {
                        snapshot
                            .agents
                            .iter()
                            .any(|agent| agent.enrollment.address == target.team_address())
                    })
                {
                    continue;
                }
                let name = thread
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&thread.preview)
                    .to_owned();
                candidates.push((format!("{} · {}", state.label, name), target));
            }
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        let items = candidates
            .into_iter()
            .map(|(name, target)| SelectionItem {
                name,
                actions: vec![Box::new(move |sender| {
                    sender
                        .unscoped()
                        .send(AppEvent::AgentsTeamCompose(TeamCompose::Alias(
                            target.clone(),
                        )))
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Enroll existing session".into()),
            subtitle: Some("Choose a root session from the loaded Tasks list. Its history and settings are kept.".into()),
            items,
            ..SelectionViewParams::picker()
        });
    }
}

fn member_models(draft: MemberDraft, models: Vec<Model>) -> SelectionViewParams {
    let defaults = draft.clone();
    let mut items = vec![SelectionItem {
        name: "Use this machine's configured defaults".into(),
        description: Some(
            "Resolve model and effort from the selected checkout's configuration".into(),
        ),
        actions: vec![Box::new(move |sender| {
            sender
                .unscoped()
                .send(AppEvent::AgentsMember(Box::new(MemberAction::Confirm(
                    defaults.clone(),
                ))))
        })],
        dismiss_on_select: true,
        ..Default::default()
    }];
    items.extend(
        models
            .into_iter()
            .filter(|model| !model.hidden)
            .map(|model| {
                let draft = draft.clone();
                SelectionItem {
                    name: model.display_name.clone(),
                    description: Some(model.description.clone()),
                    actions: vec![Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::AgentsMember(Box::new(
                            MemberAction::Model {
                                draft: draft.clone(),
                                model: Box::new(model.clone()),
                            },
                        )))
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            }),
    );
    SelectionViewParams {
        title: Some(format!("Member model · {}", draft.host_label)),
        items,
        ..SelectionViewParams::picker()
    }
}

fn member_confirmation(draft: MemberDraft) -> SelectionViewParams {
    let header = OperationHeader(vec![
        format!("{} · {}", draft.request.alias, draft.host_label)
            .bold()
            .into(),
        format!("Checkout: {}", draft.request.cwd).into(),
        format!(
            "Model: {}",
            draft
                .request
                .model
                .as_deref()
                .unwrap_or("Machine configuration")
        )
        .into(),
        format!(
            "Effort: {}",
            draft.request.effort.as_deref().unwrap_or("Default")
        )
        .into(),
        "Creates an idle member. Existing team holds still apply."
            .dim()
            .into(),
    ]);
    SelectionViewParams {
        title: Some("Create idle team member".into()),
        header: Box::new(header),
        items: vec![SelectionItem {
            name: "Create member".into(),
            actions: vec![Box::new(move |sender| {
                sender
                    .unscoped()
                    .send(AppEvent::AgentsMember(Box::new(MemberAction::Submit(
                        draft.clone(),
                    ))))
            })],
            dismiss_on_select: true,
            require_explicit_confirmation: true,
            ..Default::default()
        }],
        ..SelectionViewParams::picker()
    }
}

#[cfg(test)]
#[path = "agents_member_tests.rs"]
mod tests;
