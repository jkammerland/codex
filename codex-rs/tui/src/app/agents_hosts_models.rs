//! Model row actions retain the target host while reusing the normal model and effort views.

use super::agents_hosts::ConnectionScope;
use super::agents_hosts::ThreadTarget;
use super::*;
use crate::bottom_pane::SelectionItem;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::TeamSelection;
use codex_app_server_protocol::TeamSelectionOrigin;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;

#[derive(Clone, Debug)]
pub(crate) struct ModelPickerContext {
    pub(crate) request_id: Uuid,
    pub(crate) target: ThreadTarget,
    pub(crate) scope: ConnectionScope,
    pub(crate) generation: u64,
    pub(crate) presets: Vec<ModelPreset>,
    pub(crate) current_model: String,
    pub(crate) current_effort: Option<ReasoningEffort>,
    pub(crate) origins: Option<(TeamSelectionOrigin, TeamSelectionOrigin)>,
}

#[derive(Debug)]
pub(crate) enum ModelAction {
    CancelRequest(Uuid),
    Dismiss(ModelPickerContext),
    Open(ThreadTarget),
    Ready {
        context: ModelPickerContext,
        result: Result<(), String>,
    },
    Apply {
        context: ModelPickerContext,
        model: String,
        effort: Option<ReasoningEffort>,
    },
    Policy {
        target: ThreadTarget,
        model: TeamSelection,
        effort: TeamSelection,
    },
}

impl App {
    pub(super) fn agents_host_model_menu_items(&self, target: ThreadTarget) -> Vec<SelectionItem> {
        let open = target.clone();
        let mut items = vec![SelectionItem {
            name: "Choose task model and effort".into(),
            description: Some("Use this machine's catalog; leave saved defaults unchanged".into()),
            actions: vec![Box::new(move |sender| {
                sender
                    .unscoped()
                    .send(AppEvent::AgentsHostModel(Box::new(ModelAction::Open(
                        open.clone(),
                    ))))
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];
        let enrolled = self.agents_hosts.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.enrollment.address == target.team_address())
        });
        let inherited = enrolled
            .and_then(|agent| agent.runtime.as_ref())
            .is_some_and(|runtime| runtime.parent_thread_id.is_some());
        let mut policies = vec![(
            "Reset task effort to model default",
            TeamSelection::Keep,
            TeamSelection::Default,
        )];
        if enrolled.is_some() {
            policies.push((
                "Reset task model to configured default",
                TeamSelection::Default,
                TeamSelection::Keep,
            ));
        }
        if inherited {
            policies.push((
                "Inherit model from parent",
                TeamSelection::Inherit,
                TeamSelection::Keep,
            ));
            policies.push((
                "Inherit effort from parent",
                TeamSelection::Keep,
                TeamSelection::Inherit,
            ));
        }
        for (name, model, effort) in policies {
            let target = target.clone();
            items.push(SelectionItem {
                name: name.into(),
                actions: vec![Box::new(move |sender| {
                    sender.unscoped().send(AppEvent::AgentsHostModel(Box::new(
                        ModelAction::Policy {
                            target: target.clone(),
                            model: model.clone(),
                            effort: effort.clone(),
                        },
                    )))
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items
    }

    pub(super) fn handle_agents_host_model(
        &mut self,
        foreground: &AppServerSession,
        action: ModelAction,
    ) {
        match action {
            ModelAction::CancelRequest(request_id) => {
                self.chat_widget.cancel_agents_model_loading(request_id);
            }
            ModelAction::Dismiss(context) => {
                self.chat_widget.clear_closed_agents_model_popup(&context)
            }
            ModelAction::Open(mut target) => {
                target.host = self.agents_hosts.canonical_host(&target.host);
                let Some(handle) = self.agents_hosts.request_handle(&target.host, foreground)
                else {
                    return;
                };
                let Some(host) = self.agents_hosts.hosts.get(&target.host) else {
                    return;
                };
                let scope = ConnectionScope {
                    host: target.host.clone(),
                    epoch: host.epoch,
                };
                let generation = self.agents_hosts.generation;
                let request_id = Uuid::new_v4();
                self.chat_widget.begin_agents_model_popup(request_id);
                let effort = self
                    .agents_hosts
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .agents
                            .iter()
                            .find(|agent| agent.enrollment.address == target.team_address())
                    })
                    .and_then(|agent| agent.next_effort.as_ref())
                    .and_then(|effort| {
                        serde_json::from_value(serde_json::Value::String(effort.clone())).ok()
                    });
                let origins = self
                    .agents_hosts
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .agents
                            .iter()
                            .find(|agent| agent.enrollment.address == target.team_address())
                    })
                    .and_then(|agent| agent.runtime.as_ref())
                    .map(|runtime| (runtime.model_origin, runtime.effort_origin));
                let sender = self.app_event_tx.unscoped();
                self.agents_hosts.spawn(async move {
                    let mut context = ModelPickerContext {
                        request_id,
                        target,
                        scope,
                        generation,
                        presets: Vec::new(),
                        current_model: String::new(),
                        current_effort: effort,
                        origins,
                    };
                    let result = async {
                        let mut cursor = None;
                        loop {
                            let page: ModelListResponse = handle
                                .request_typed(ClientRequest::ModelList {
                                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                                    params: ModelListParams {
                                        cursor,
                                        limit: None,
                                        include_hidden: Some(true),
                                    },
                                })
                                .await?;
                            context.presets.extend(
                                page.data
                                    .into_iter()
                                    .map(crate::app_server_session::model_preset_from_api_model),
                            );
                            let Some(next) = page.next_cursor else {
                                break;
                            };
                            cursor = Some(next);
                        }
                        let thread: ThreadReadResponse = handle
                            .request_typed(ClientRequest::ThreadRead {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadReadParams {
                                    thread_id: context.target.thread_id.to_string(),
                                    include_turns: false,
                                },
                            })
                            .await?;
                        context.current_model = thread.thread.model.unwrap_or_default();
                        context.current_effort = thread.thread.reasoning_effort;
                        Ok::<_, codex_app_server_client::TypedRequestError>(())
                    }
                    .await
                    .map_err(|error| error.to_string());
                    sender.send(AppEvent::AgentsHostModel(Box::new(ModelAction::Ready {
                        context,
                        result,
                    })));
                });
            }
            ModelAction::Ready { context, result } => {
                let present = self
                    .chat_widget
                    .finish_agents_model_loading(context.request_id);
                if !self.agents_hosts.accepts(&context.scope)
                    || self.agents_hosts.generation != context.generation
                    || !present
                {
                    return;
                }
                match result {
                    Ok(()) => self.chat_widget.open_agents_model_popup(context),
                    Err(error) => self.add_agents_overview_error(error),
                }
            }
            ModelAction::Apply {
                context,
                model,
                effort,
            } => {
                if !self.agents_hosts.accepts(&context.scope) {
                    return;
                }
                self.chat_widget.clear_closed_agents_model_popup(&context);
                self.apply_agents_host_model(
                    foreground,
                    context.target,
                    TeamSelection::Value { value: model },
                    effort.map_or(TeamSelection::Default, |effort| TeamSelection::Value {
                        value: effort.to_string(),
                    }),
                );
            }
            ModelAction::Policy {
                target,
                model,
                effort,
            } => self.apply_agents_host_model(foreground, target, model, effort),
        }
    }

    fn apply_agents_host_model(
        &mut self,
        foreground: &AppServerSession,
        mut target: ThreadTarget,
        model: TeamSelection,
        effort: TeamSelection,
    ) {
        target.host = self.agents_hosts.canonical_host(&target.host);
        let enrolled = self.agents_hosts.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .agents
                .iter()
                .any(|agent| agent.enrollment.address == target.team_address())
        });
        if enrolled {
            self.submit_agents_team_request(codex_team::TeamRequest::SelectModel {
                address: target.team_address(),
                model,
                effort,
            });
            return;
        }
        let Some(handle) = self.agents_hosts.request_handle(&target.host, foreground) else {
            return;
        };
        let Some(host) = self.agents_hosts.hosts.get(&target.host) else {
            return;
        };
        let scope = ConnectionScope {
            host: target.host,
            epoch: host.epoch,
        };
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.spawn(async move {
            let result = async {
                // This explicit edit may load its one task; discovery never does.
                handle
                    .request_typed::<ThreadResumeResponse>(ClientRequest::ThreadResume {
                        request_id: RequestId::String(Uuid::new_v4().to_string()),
                        params: ThreadResumeParams {
                            thread_id: target.thread_id.to_string(),
                            ..Default::default()
                        },
                    })
                    .await?;
                handle
                    .request_typed::<ThreadSettingsUpdateResponse>(
                        ClientRequest::ThreadSettingsUpdate {
                            request_id: RequestId::String(Uuid::new_v4().to_string()),
                            params: ThreadSettingsUpdateParams {
                                thread_id: target.thread_id.to_string(),
                                model: match model {
                                    TeamSelection::Value { value } => Some(value),
                                    _ => None,
                                },
                                effort_policy: Some(effort),
                                ..Default::default()
                            },
                        },
                    )
                    .await?;
                Ok::<_, codex_app_server_client::TypedRequestError>(())
            }
            .await
            .map_err(|error| error.to_string());
            sender.send(AppEvent::AgentsHostUpdate(Box::new(
                super::agents_hosts::HostUpdate::ActionFinished { scope, result },
            )));
        });
    }
}
