//! Row actions use the selected machine's connection without changing the foreground.

use super::agents_hosts::ConnectionScope;
use super::agents_hosts::HostUpdate;
use super::agents_hosts::ThreadTarget;
use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSectionMoveParams;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStatus;

impl App {
    pub(super) fn handle_background_agents_action(
        &mut self,
        target: ThreadTarget,
        event: AppEvent,
        foreground: &mut AppServerSession,
    ) {
        let Some(handle) = self.agents_hosts.request_handle(&target.host, foreground) else {
            return;
        };
        let Some(host) = self.agents_hosts.hosts.get_mut(&target.host) else {
            return;
        };
        let scope = ConnectionScope {
            host: target.host.clone(),
            epoch: host.epoch,
        };
        let thread_id = target.thread_id;
        let lifecycle = matches!(event, AppEvent::RunAgentsOverviewAction { .. });
        let request = match event {
            AppEvent::HideAgentsOverviewThread { .. } => {
                host.state.agents_overview.hidden_threads.insert(thread_id);
                self.repaint_agents_overview();
                return;
            }
            AppEvent::ConfirmAgentsOverviewAction { action, .. } => {
                let title = match action {
                    crate::app_event::AgentsOverviewAction::Archive => {
                        "Archive task and child agents?"
                    }
                    crate::app_event::AgentsOverviewAction::Delete => {
                        "Permanently delete task and child agents?"
                    }
                };
                self.chat_widget.show_selection_view(SelectionViewParams {
                    title: Some(title.into()),
                    subtitle: Some(
                        "Running work is stopped. Delete permanently removes the saved history."
                            .into(),
                    ),
                    items: vec![
                        SelectionItem {
                            name: "Cancel".into(),
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: title.trim_end_matches('?').into(),
                            dismiss_on_select: true,
                            require_explicit_confirmation: action
                                == crate::app_event::AgentsOverviewAction::Delete,
                            actions: vec![Box::new(move |sender| {
                                sender.unscoped().send(AppEvent::AgentsHostAction {
                                    target: target.clone(),
                                    event: Box::new(AppEvent::RunAgentsOverviewAction {
                                        thread_id,
                                        action,
                                    }),
                                })
                            })],
                            ..Default::default()
                        },
                    ],
                    ..SelectionViewParams::confirmation()
                });
                return;
            }
            AppEvent::RenameAgentsOverviewThread { name, .. } => {
                Some(ClientRequest::ThreadSetName {
                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                    params: codex_app_server_protocol::ThreadSetNameParams {
                        thread_id: thread_id.to_string(),
                        name,
                    },
                })
            }
            AppEvent::RunAgentsOverviewAction { action, .. } => Some(match action {
                crate::app_event::AgentsOverviewAction::Archive => ClientRequest::ThreadArchive {
                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                    params: codex_app_server_protocol::ThreadArchiveParams {
                        thread_id: thread_id.to_string(),
                    },
                },
                crate::app_event::AgentsOverviewAction::Delete => ClientRequest::ThreadDelete {
                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                    params: codex_app_server_protocol::ThreadDeleteParams {
                        thread_id: thread_id.to_string(),
                    },
                },
            }),
            AppEvent::ToggleAgentsOverviewPin { pinned, .. } => {
                Some(ClientRequest::ThreadSectionMove {
                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                    params: ThreadSectionMoveParams {
                        thread_id: thread_id.to_string(),
                        section_id: pinned
                            .then(|| codex_state::PINNED_THREAD_SECTION_ID.to_string()),
                        before_thread_id: None,
                    },
                })
            }
            AppEvent::StopAgentsOverviewThread { .. } => {
                let sender = self.app_event_tx.unscoped();
                self.agents_hosts.spawn(async move {
                    let result = async {
                        let response: ThreadReadResponse = handle
                            .request_typed(ClientRequest::ThreadRead {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadReadParams {
                                    thread_id: thread_id.to_string(),
                                    include_turns: true,
                                },
                            })
                            .await?;
                        for turn in response
                            .thread
                            .turns
                            .into_iter()
                            .filter(|turn| turn.status == TurnStatus::InProgress)
                        {
                            handle
                                .request_typed::<serde_json::Value>(ClientRequest::TurnInterrupt {
                                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                                    params: TurnInterruptParams {
                                        thread_id: thread_id.to_string(),
                                        turn_id: turn.id,
                                    },
                                })
                                .await?;
                        }
                        Ok::<_, codex_app_server_client::TypedRequestError>(())
                    }
                    .await
                    .map_err(|_| "Could not stop this machine's task".into());
                    sender.send(AppEvent::AgentsHostUpdate(Box::new(
                        HostUpdate::ActionFinished { scope, result },
                    )));
                });
                return;
            }
            AppEvent::NewAgentsOverviewSession { cwd } => {
                self.start_other_host_task(target.host, cwd, /*fork_parent*/ None, foreground);
                return;
            }
            AppEvent::ForkAgentsOverviewThread { .. } => {
                self.start_other_host_task(
                    target.host,
                    /*cwd*/ None,
                    Some(target.thread_id),
                    foreground,
                );
                return;
            }
            AppEvent::NewAgentsOverviewWorktree { .. } => {
                self.agents_hosts.notice = Some(
                    "Create the worktree on the selected machine, then start a task in its folder"
                        .into(),
                );
                self.repaint_agents_overview();
                return;
            }
            _ => None,
        };
        let Some(request) = request else { return };
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.spawn(async move {
            let result = handle
                .request_typed::<serde_json::Value>(request)
                .await
                .map(|_| ())
                .map_err(|_| "Machine task action failed".into());
            let update = if lifecycle {
                HostUpdate::LifecycleFinished {
                    scope,
                    thread_id,
                    result,
                }
            } else {
                HostUpdate::ActionFinished { scope, result }
            };
            sender.send(AppEvent::AgentsHostUpdate(Box::new(update)));
        });
    }
}
