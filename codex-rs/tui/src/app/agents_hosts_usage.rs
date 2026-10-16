//! Usage reads follow the selected row's machine, independently of foreground account state.

use super::agents_hosts::ConnectionScope;
use super::agents_hosts::HostUpdate;
use super::agents_hosts::ThreadTarget;
use super::agents_overview_usage::REFRESH_INTERVAL;
use super::*;

impl App {
    pub(super) fn refresh_other_agents_host_usage(
        &mut self,
        foreground: &AppServerSession,
        frame_requester: crate::tui::FrameRequester,
        target: ThreadTarget,
    ) {
        let Some(handle) = self.agents_hosts.request_handle(&target.host, foreground) else {
            return;
        };
        let Some(host) = self.agents_hosts.hosts.get_mut(&target.host) else {
            return;
        };
        let overview = &mut host.state.agents_overview;
        if !host.connected || overview.usage_disabled || overview.pending_usage.is_some() {
            return;
        }
        if let Some(age) = overview
            .usage
            .get(&target.thread_id)
            .and_then(|usage| usage.fetched_at)
            .map(|fetched| fetched.elapsed())
            && age < REFRESH_INTERVAL
        {
            frame_requester.schedule_frame_in(REFRESH_INTERVAL - age);
            return;
        }
        let request_id = Uuid::new_v4();
        overview.pending_usage = Some((target.thread_id, request_id));
        let scope = ConnectionScope {
            host: target.host,
            epoch: host.epoch,
        };
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.spawn(async move {
            let result = super::background_requests::fetch_thread_usage(handle, target.thread_id)
                .await
                .map_err(|error| error.to_string());
            sender.send(AppEvent::AgentsHostUpdate(Box::new(
                HostUpdate::UsageLoaded {
                    scope,
                    thread_id: target.thread_id,
                    request_id,
                    result,
                },
            )));
        });
    }
}
