//! Applies a completed lifecycle mutation to its owning host without blocking discovery elsewhere.

use super::agents_hosts::ConnectionScope;
use super::*;

fn remove_family(overview: &mut agents_overview::AgentsOverviewState, root: ThreadId) {
    let mut removed = HashSet::from([root]);
    loop {
        let count = removed.len();
        for (id, thread) in &overview.threads {
            if let Some(thread) = thread
                && (thread.session_id == root.to_string()
                    || thread
                        .parent_thread_id
                        .as_deref()
                        .and_then(|parent| ThreadId::from_string(parent).ok())
                        .is_some_and(|parent| removed.contains(&parent)))
            {
                removed.insert(*id);
            }
        }
        if removed.len() == count {
            break;
        }
    }
    for id in removed {
        overview.removed_threads.insert(id);
        overview.threads.remove(&id);
        overview.activity.remove(&id);
        overview.last_messages.remove(&id);
        overview.usage.remove(&id);
        overview.input_states.remove(&id);
        overview.dispatched_requests.remove(&id);
    }
}

impl App {
    pub(super) async fn finish_host_lifecycle(
        &mut self,
        tui: &mut tui::Tui,
        foreground: &mut AppServerSession,
        scope: ConnectionScope,
        thread_id: ThreadId,
        result: Result<(), String>,
    ) -> Result<()> {
        if !self.agents_hosts.accepts(&scope) {
            return Ok(());
        }
        if let Err(error) = result {
            self.agents_hosts.notice = Some(error);
            return Ok(());
        }
        if scope.host == self.agents_hosts.foreground {
            let removes_primary = self.primary_thread_id.is_some_and(|id| {
                id == thread_id
                    || self
                        .agents_overview
                        .threads
                        .get(&id)
                        .and_then(Option::as_ref)
                        .is_some_and(|thread| thread.session_id == thread_id.to_string())
            });
            self.prepare_agents_overview_removal(&HashSet::from([thread_id]));
            remove_family(&mut self.agents_overview, thread_id);
            if removes_primary {
                self.reset_for_thread_switch(tui)?;
                self.pending_thread_switch_resets += 1;
                self.app_event_tx
                    .send(AppEvent::ResetTranscriptForThreadSwitch);
                self.reset_thread_event_state().await;
                let init = self.chatwidget_init_for_forked_or_resumed_thread(
                    tui,
                    self.config.clone(),
                    /*initial_user_message*/ None,
                );
                self.replace_chat_widget(ChatWidget::new_with_app_event(init));
                self.open_agents_overview(foreground);
            }
            self.refresh_agents_overview_threads(foreground);
        } else if let Some(host) = self.agents_hosts.hosts.get_mut(&scope.host) {
            remove_family(&mut host.state.agents_overview, thread_id);
        }
        self.repaint_agents_overview();
        Ok(())
    }
}
