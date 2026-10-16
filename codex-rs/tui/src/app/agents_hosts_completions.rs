//! Safe asynchronous lifecycle completions stay with their originating host.
//! Interactive actions remain fenced by the foreground generation.

use super::agents_hosts::ConnectionScope;
use super::*;
use crate::temporary_structured_request::run_temporary_structured_turn;
use crate::temporary_structured_request::unsubscribe_temporary_thread;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSetNameParams;

fn retained_completion(event: &AppEvent) -> bool {
    matches!(
        event,
        AppEvent::AgentsOverviewThreadsLoaded { .. }
            | AppEvent::AgentsOverviewUsageLoaded { .. }
            | AppEvent::AgentsOverviewPinToggled { .. }
            | AppEvent::AccountEmailLoaded { .. }
            | AppEvent::RateLimitsLoaded { .. }
            | AppEvent::RateLimitResetCreditConsumed { .. }
            | AppEvent::PluginEnabledSet { .. }
            | AppEvent::HookEnabledSet { .. }
            | AppEvent::HookTrusted { .. }
            | AppEvent::DynamicToolThreadStarted { .. }
            | AppEvent::DynamicToolCallCompleted { .. }
            | AppEvent::TaskToolsAvailable { .. }
            | AppEvent::ThreadTitleStarted { .. }
            | AppEvent::GeneratedThreadTitle { .. }
            | AppEvent::McpLoginStarted { .. }
            | AppEvent::UserVerificationFinished { .. }
            | AppEvent::CompleteHostPromptRevert { .. }
            | AppEvent::FinishPromptRevert { .. }
            | AppEvent::RecapStarted { .. }
            | AppEvent::RecapGenerated { .. }
            | AppEvent::ManagedWorktreeCreated(_)
    )
}

impl App {
    pub(super) fn recover_host_completion(
        &mut self,
        scope: ConnectionScope,
        event: AppEvent,
    ) -> Option<AppEvent> {
        if !self.agents_hosts.accepts(&scope) || !retained_completion(&event) {
            return None;
        }
        if scope.host == self.agents_hosts.foreground {
            return Some(event);
        }
        if matches!(event, AppEvent::FinishPromptRevert { .. }) {
            // The old transcript is replaced on attach; release its global input barrier now.
            self.pending_thread_switch_resets = self.pending_thread_switch_resets.saturating_sub(1);
            return None;
        }
        let handle = self
            .agents_hosts
            .hosts
            .get(&scope.host)?
            .session
            .as_ref()?
            .request_handle();
        let host = self.agents_hosts.hosts.get_mut(&scope.host)?;
        match event {
            AppEvent::RecapStarted { result, .. } => {
                host.state.recap.stop_ui_tasks();
                if let Ok(id) = result {
                    self.agents_hosts.spawn(async move {
                        unsubscribe_temporary_thread(&handle, id).await;
                    });
                }
            }
            AppEvent::RecapGenerated {
                temporary_thread_id,
                ..
            } => {
                host.state
                    .temporary_structured_requests
                    .remove(&temporary_thread_id);
                host.state.recap.stop_ui_tasks();
            }
            AppEvent::DynamicToolCallCompleted { request_id, .. } => {
                // The ingress sender already returned the response to its original connection.
                host.state.dynamic_tool_tasks.remove(&request_id);
                host.state.dynamic_tool_origins.remove(&request_id);
            }
            AppEvent::TaskToolsAvailable { thread_id } => {
                host.session.as_mut()?.remember_task_tool_thread(thread_id);
            }
            AppEvent::DynamicToolThreadStarted {
                mut thread,
                task_tools_available,
                registered,
            } => {
                if let Ok(id) = ThreadId::from_string(&thread.id) {
                    host.state
                        .agents_overview
                        .dispatched_requests
                        .entry(id)
                        .or_default();
                    if task_tools_available {
                        host.session.as_mut()?.remember_task_tool_thread(id);
                    }
                    if !thread.ephemeral
                        && !host.state.agents_overview.removed_threads.contains(&id)
                        && !host
                            .state
                            .agents_overview
                            .threads
                            .get(&id)
                            .is_some_and(Option::is_some)
                    {
                        super::agents_hosts::state::normalize_thread(&mut thread);
                        host.state.agents_overview.threads.insert(id, Some(thread));
                    }
                }
                let _ = registered.send(());
            }
            AppEvent::ThreadTitleStarted {
                cancellation,
                thread_id,
                destination,
                prompt,
                effort,
                result,
            } => {
                if cancellation.is_cancelled() {
                    host.state
                        .pending_thread_titles
                        .remove(&(thread_id, destination));
                    if let Ok(id) = result {
                        self.agents_hosts.spawn(async move {
                            unsubscribe_temporary_thread(&handle, id).await;
                        });
                    }
                    return None;
                }
                let Ok(temporary_id) = result else {
                    host.state
                        .pending_thread_titles
                        .remove(&(thread_id, destination));
                    return None;
                };
                let Ok(temporary_thread_id) = ThreadId::from_string(&temporary_id) else {
                    host.state
                        .pending_thread_titles
                        .remove(&(thread_id, destination));
                    return None;
                };
                let (sender, receiver) = mpsc::unbounded_channel();
                host.state
                    .temporary_structured_requests
                    .insert(temporary_thread_id, sender);
                let sender = self.app_event_tx.unscoped().scoped(scope, 0);
                self.agents_hosts.spawn(async move {
                    let result = run_temporary_structured_turn(
                        handle,
                        temporary_id,
                        prompt,
                        super::thread_title::thread_title_output_schema(),
                        effort,
                        receiver,
                        cancellation.clone(),
                    )
                    .await
                    .map_err(|error| error.to_string());
                    sender.send(AppEvent::GeneratedThreadTitle {
                        cancellation,
                        thread_id,
                        temporary_thread_id,
                        destination,
                        result,
                    });
                });
            }
            AppEvent::GeneratedThreadTitle {
                cancellation,
                thread_id,
                temporary_thread_id,
                destination,
                result,
            } => {
                host.state
                    .temporary_structured_requests
                    .remove(&temporary_thread_id);
                host.state
                    .pending_thread_titles
                    .remove(&(thread_id, destination));
                if cancellation.is_cancelled() {
                    return None;
                }
                if destination == ThreadTitleDestination::Automatic
                    && let Ok(response) = result
                    && let Some(title) = super::thread_title::parse_thread_title(&response)
                {
                    self.agents_hosts.spawn(async move {
                        let response = handle
                            .request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                                request_id: RequestId::String(Uuid::new_v4().to_string()),
                                params: ThreadReadParams {
                                    thread_id: thread_id.to_string(),
                                    include_turns: false,
                                },
                            })
                            .await;
                        if response.is_ok_and(|response| response.thread.name.is_none()) {
                            let _ = handle
                                .request_typed::<serde_json::Value>(ClientRequest::ThreadSetName {
                                    request_id: RequestId::String(Uuid::new_v4().to_string()),
                                    params: ThreadSetNameParams {
                                        thread_id: thread_id.to_string(),
                                        name: title,
                                    },
                                })
                                .await;
                        }
                    });
                }
            }
            event => host.state.pending_app_events.push_back(AppEvent::Scoped {
                scope,
                generation: 0,
                event: Box::new(event),
            }),
        }
        None
    }
}
