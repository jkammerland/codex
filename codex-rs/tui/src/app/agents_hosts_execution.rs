//! Project coordinator admission onto the selected machine/server/thread without changing its
//! ordinary manual-resume semantics. Request errors remain typed until they are classified.

use super::*;
use crate::chatwidget::team_execution::TeamExecution;
use codex_app_server_protocol::TeamExecutionMode;

impl App {
    pub(super) fn team_execution_for_thread(&self, thread_id: ThreadId) -> Option<TeamExecution> {
        let host = &self.agents_hosts.foreground;
        if self
            .agents_hosts
            .held_rejections
            .contains_key(&agents_hosts::ThreadTarget {
                host: host.clone(),
                thread_id,
            })
        {
            return Some(
                match self
                    .agents_hosts
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.control.mode)
                {
                    Some(TeamExecutionMode::Stopped) => TeamExecution::Stopped,
                    Some(TeamExecutionMode::Paused) => TeamExecution::Paused,
                    Some(TeamExecutionMode::Running) | None => TeamExecution::Syncing,
                },
            );
        }
        let snapshot = self.agents_hosts.snapshot.as_ref()?;
        let agent = snapshot.agents.iter().find(|agent| {
            let address = &agent.enrollment.address;
            address.machine_id == host.machine_id
                && address.server_id == host.server_id
                && address.thread_id == thread_id.to_string()
        })?;
        Some(match snapshot.control.mode {
            TeamExecutionMode::Stopped => TeamExecution::Stopped,
            TeamExecutionMode::Paused => TeamExecution::Paused,
            TeamExecutionMode::Running => {
                let applied = snapshot.servers.iter().any(|server| {
                    server.machine_id == host.machine_id
                        && server.server_id == host.server_id
                        && server.connected
                        && server
                            .applied_revision
                            .is_some_and(|revision| revision >= snapshot.control.revision)
                });
                if !applied {
                    TeamExecution::Syncing
                } else if agent
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.reconciling)
                {
                    TeamExecution::Resuming
                } else {
                    // Keep ordinary prompts available to resume this task's own manual hold.
                    // The server still validates inherited holds.
                    TeamExecution::Running
                }
            }
        })
    }

    pub(super) async fn restore_team_held_submission(
        &mut self,
        thread_id: ThreadId,
        client_id: &str,
    ) {
        // Reusing the same accepted Running snapshot after navigation cannot override a newer
        // rejection. Fence already-issued reads too; only a read begun after rejection releases it.
        self.agents_hosts.held_rejections.insert(
            agents_hosts::ThreadTarget {
                host: self.agents_hosts.foreground.clone(),
                thread_id,
            },
            self.agents_hosts.applied_team_read.max(
                self.agents_hosts
                    .team_read_generation
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        );
        self.sync_chat_team_execution();
        if self.chat_widget.thread_id() == Some(thread_id)
            && self.chat_widget.restore_team_held_submission(client_id)
        {
            return;
        }

        // Ordinary navigation replays the event store; host attachment may instead retain an
        // overview copy. Reconcile both copies before returning to either navigation path.
        let mut recovered = false;
        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            recovered |= store
                .input_state
                .as_mut()
                .is_some_and(|state| state.restore_team_held_submission(client_id));
        }
        recovered |= self
            .agents_overview
            .input_states
            .get_mut(&thread_id)
            .is_some_and(|state| state.restore_team_held_submission(client_id));

        // Voice ownership transfers only realtime state on return. Preserve its restored draft
        // in the navigation cache too when no earlier snapshot contained this submission.
        if let Some(voice) = self.background_voice.as_mut()
            && voice.thread_id() == Some(thread_id)
            && voice.restore_team_held_submission(client_id)
        {
            if !recovered {
                let state = voice.capture_thread_input_state();
                if let Some(channel) = self.thread_event_channels.get(&thread_id) {
                    channel.store.lock().await.input_state = state;
                } else if let Some(state) = state {
                    self.agents_overview.input_states.insert(thread_id, state);
                }
            }
            recovered = true;
        }
        let notice = if recovered {
            "Not sent; draft restored in its task"
        } else {
            // Programmatic or already-settled requests may have no editable draft.
            "Team held; request was not sent"
        };
        self.chat_widget.add_info_message(notice.into(), None);
    }

    pub(super) fn sync_chat_team_execution(&mut self) {
        let execution = self
            .current_displayed_thread_id()
            .and_then(|thread_id| self.team_execution_for_thread(thread_id));
        self.chat_widget.set_team_execution(execution);
    }
}

pub(super) fn team_execution_held_error(error: &TypedRequestError) -> bool {
    let TypedRequestError::Server { method, source } = error else {
        return false;
    };
    if !matches!(method.as_str(), "turn/start" | "turn/steer") {
        return false;
    }
    if source.code == -32600 {
        return source
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            == Some("teamExecutionHeld");
    }
    // Older app-servers used one exact internal-error string per request method.
    source.code == -32603
        && source.data.is_none()
        && source.message
            == match method.as_str() {
                "turn/start" => {
                    "failed to submit turn input: session execution is held by team control"
                }
                "turn/steer" => "failed to steer turn: session execution is held by team control",
                _ => return false,
            }
}
