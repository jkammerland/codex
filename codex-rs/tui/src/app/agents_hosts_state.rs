//! State retained by its machine when another machine owns the visible widget.

use super::*;

#[derive(Default)]
pub(in crate::app) struct HostState {
    pub(in crate::app) config: Option<Config>,
    pub(in crate::app) local_settings: Option<crate::local_settings::LocalSettings>,
    model_catalog: Option<Arc<ModelCatalog>>,
    runtime_working_directory_override: Option<PathBuf>,
    pending_open_resume_picker: bool,
    pending_working_directory_change: Option<working_directory::PendingWorkingDirectoryChange>,
    pending_start_managed_worktree: Option<(crate::app_event::ManagedWorktreeMode, Option<String>)>,
    pending_managed_worktree_creation: bool,
    pending_managed_worktree_created: Option<Box<crate::app_event::ManagedWorktreeCreated>>,
    pending_managed_worktree_transition: Option<Box<crate::app_event::ManagedWorktreeTransition>>,
    pending_managed_worktree_attach: Option<Box<working_directory::ManagedWorktreeAttach>>,
    pending_startup_thread_start: bool,
    pending_mcp_login_start: Option<PendingMcpLoginStart>,
    active_mcp_login_ids: HashMap<String, String>,
    pub(in crate::app) recap: recap::RecapState,
    pub(in crate::app) pending_host_prompt_reverts:
        HashMap<ThreadId, super::agents_hosts_revert::PendingHostPromptRevert>,

    runtime_approval_policy_override: Option<RuntimeApprovalPolicyOverride>,
    runtime_approvals_reviewer_override: Option<ApprovalsReviewer>,
    runtime_permission_profile_override: Option<RuntimePermissionProfileOverride>,
    pending_plugin_enabled_writes: HashMap<String, Option<bool>>,
    pending_hook_enabled_writes: HashMap<String, Option<bool>>,
    pub(in crate::app) pending_app_events: VecDeque<AppEvent>,
    thread_event_channels: HashMap<ThreadId, ThreadEventChannel>,
    pending_realtime_speech_replay: HashMap<ThreadId, Vec<(String, ThreadItem)>>,
    pending_realtime_transcript_replay:
        HashMap<ThreadId, VecDeque<crate::chatwidget::RealtimeTranscriptRecord>>,
    realtime_replay_order: VecDeque<ThreadId>,
    background_voice: Option<Box<ChatWidget>>,
    background_voice_error: Option<(ThreadId, String)>,
    pub(in crate::app) temporary_structured_requests:
        HashMap<ThreadId, mpsc::UnboundedSender<ServerNotification>>,
    /// Track title generation across thread switches and deduplicate automatic requests.
    pub(in crate::app) pending_thread_titles:
        HashMap<(ThreadId, ThreadTitleDestination), CancellationToken>,
    thread_event_listener_tasks: HashMap<ThreadId, JoinHandle<()>>,
    agent_navigation: AgentNavigationState,
    pub(in crate::app) agents_overview: agents_overview::AgentsOverviewState,
    side_threads: HashMap<ThreadId, SideThreadState>,
    abandoned_side_threads: HashSet<ThreadId>,
    active_thread_id: Option<ThreadId>,
    active_thread_rx: Option<mpsc::Receiver<ThreadBufferedEvent>>,
    primary_thread_id: Option<ThreadId>,
    last_subagent_backfill_attempt: Option<ThreadId>,
    primary_session_configured: Option<ThreadSessionState>,
    pending_primary_events: VecDeque<ThreadBufferedEvent>,
    pub(in crate::app) pending_app_server_requests: PendingAppServerRequests,
    pub(in crate::app) dynamic_tool_status_updates: Option<
        tokio::sync::broadcast::Sender<codex_app_server_protocol::ThreadStatusChangedNotification>,
    >,
    pub(in crate::app) dynamic_tool_tasks:
        HashMap<codex_app_server_protocol::RequestId, (String, JoinHandle<()>)>,
    pub(in crate::app) dynamic_tool_origins: HashMap<
        codex_app_server_protocol::RequestId,
        codex_app_server_client::AppServerReplyHandle,
    >,
    reconnect: reconnect::ReconnectState,
    pending_server_profiles: HashMap<ThreadId, PermissionProfileSelection>,
    rate_limit_refresh_state: rate_limit_refresh::RateLimitRefreshState,
    account_email_request_id: Option<Uuid>,
    rate_limit_hard_stop_generation: u64,
}

impl HostState {
    pub(in crate::app) fn swap(&mut self, app: &mut App) {
        let config = self.config.take().unwrap_or_else(|| app.config.clone());
        self.config = Some(std::mem::replace(&mut app.config, config));
        let local_settings = self
            .local_settings
            .take()
            .unwrap_or_else(|| app.local_settings.clone());
        self.local_settings = Some(std::mem::replace(&mut app.local_settings, local_settings));
        let model_catalog = self
            .model_catalog
            .take()
            .unwrap_or_else(|| Arc::clone(&app.model_catalog));
        self.model_catalog = Some(std::mem::replace(&mut app.model_catalog, model_catalog));
        std::mem::swap(
            &mut self.runtime_working_directory_override,
            &mut app.runtime_working_directory_override,
        );
        std::mem::swap(
            &mut self.pending_open_resume_picker,
            &mut app.pending_open_resume_picker,
        );
        std::mem::swap(
            &mut self.pending_working_directory_change,
            &mut app.pending_working_directory_change,
        );
        std::mem::swap(
            &mut self.pending_start_managed_worktree,
            &mut app.pending_start_managed_worktree,
        );
        std::mem::swap(
            &mut self.pending_managed_worktree_creation,
            &mut app.pending_managed_worktree_creation,
        );
        std::mem::swap(
            &mut self.pending_managed_worktree_created,
            &mut app.pending_managed_worktree_created,
        );
        std::mem::swap(
            &mut self.pending_managed_worktree_transition,
            &mut app.pending_managed_worktree_transition,
        );
        std::mem::swap(
            &mut self.pending_managed_worktree_attach,
            &mut app.pending_managed_worktree_attach,
        );
        std::mem::swap(
            &mut self.pending_startup_thread_start,
            &mut app.pending_startup_thread_start,
        );
        std::mem::swap(
            &mut self.pending_mcp_login_start,
            &mut app.pending_mcp_login_start,
        );
        std::mem::swap(
            &mut self.active_mcp_login_ids,
            &mut app.active_mcp_login_ids,
        );
        std::mem::swap(&mut self.recap, &mut app.recap);
        std::mem::swap(
            &mut self.pending_host_prompt_reverts,
            &mut app.pending_host_prompt_reverts,
        );
        std::mem::swap(
            &mut self.runtime_approval_policy_override,
            &mut app.runtime_approval_policy_override,
        );
        std::mem::swap(
            &mut self.runtime_approvals_reviewer_override,
            &mut app.runtime_approvals_reviewer_override,
        );
        std::mem::swap(
            &mut self.runtime_permission_profile_override,
            &mut app.runtime_permission_profile_override,
        );
        std::mem::swap(
            &mut self.pending_plugin_enabled_writes,
            &mut app.pending_plugin_enabled_writes,
        );
        std::mem::swap(
            &mut self.pending_hook_enabled_writes,
            &mut app.pending_hook_enabled_writes,
        );
        std::mem::swap(
            &mut self.thread_event_channels,
            &mut app.thread_event_channels,
        );
        std::mem::swap(
            &mut self.pending_realtime_speech_replay,
            &mut app.pending_realtime_speech_replay,
        );
        std::mem::swap(
            &mut self.pending_realtime_transcript_replay,
            &mut app.pending_realtime_transcript_replay,
        );
        std::mem::swap(
            &mut self.realtime_replay_order,
            &mut app.realtime_replay_order,
        );
        std::mem::swap(&mut self.background_voice, &mut app.background_voice);
        std::mem::swap(
            &mut self.background_voice_error,
            &mut app.background_voice_error,
        );
        std::mem::swap(
            &mut self.temporary_structured_requests,
            &mut app.temporary_structured_requests,
        );
        std::mem::swap(
            &mut self.pending_thread_titles,
            &mut app.pending_thread_titles,
        );
        std::mem::swap(
            &mut self.thread_event_listener_tasks,
            &mut app.thread_event_listener_tasks,
        );
        std::mem::swap(&mut self.agent_navigation, &mut app.agent_navigation);
        std::mem::swap(&mut self.agents_overview, &mut app.agents_overview);
        std::mem::swap(&mut self.side_threads, &mut app.side_threads);
        std::mem::swap(
            &mut self.abandoned_side_threads,
            &mut app.abandoned_side_threads,
        );
        std::mem::swap(&mut self.active_thread_id, &mut app.active_thread_id);
        std::mem::swap(&mut self.active_thread_rx, &mut app.active_thread_rx);
        std::mem::swap(&mut self.primary_thread_id, &mut app.primary_thread_id);
        std::mem::swap(
            &mut self.last_subagent_backfill_attempt,
            &mut app.last_subagent_backfill_attempt,
        );
        std::mem::swap(
            &mut self.primary_session_configured,
            &mut app.primary_session_configured,
        );
        std::mem::swap(
            &mut self.pending_primary_events,
            &mut app.pending_primary_events,
        );
        std::mem::swap(
            &mut self.pending_app_server_requests,
            &mut app.pending_app_server_requests,
        );
        let status_updates = self
            .dynamic_tool_status_updates
            .take()
            .unwrap_or_else(|| tokio::sync::broadcast::channel(/*capacity*/ 64).0);
        self.dynamic_tool_status_updates = Some(std::mem::replace(
            &mut app.dynamic_tool_status_updates,
            status_updates,
        ));
        std::mem::swap(&mut self.dynamic_tool_tasks, &mut app.dynamic_tool_tasks);
        std::mem::swap(
            &mut self.dynamic_tool_origins,
            &mut app.dynamic_tool_origins,
        );
        std::mem::swap(&mut self.reconnect, &mut app.reconnect);
        std::mem::swap(
            &mut self.pending_server_profiles,
            &mut app.pending_server_profiles,
        );
        std::mem::swap(
            &mut self.rate_limit_refresh_state,
            &mut app.rate_limit_refresh_state,
        );
        std::mem::swap(
            &mut self.account_email_request_id,
            &mut app.account_email_request_id,
        );
        std::mem::swap(
            &mut self.rate_limit_hard_stop_generation,
            &mut app.rate_limit_hard_stop_generation,
        );
    }
}

pub(in crate::app) fn normalize_thread(thread: &mut codex_app_server_protocol::Thread) {
    thread.turns.clear();
    if thread.parent_thread_id.is_none()
        && let codex_app_server_protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            },
        ) = &thread.source
    {
        thread.parent_thread_id = Some(parent_thread_id.to_string());
    }
}

pub(in crate::app) fn track_notification(
    state: &mut agents_overview::AgentsOverviewState,
    notification: &ServerNotification,
) {
    use super::super::app_server_event_targets::ServerNotificationThreadTarget;
    use super::super::app_server_event_targets::server_notification_thread_target;
    let ServerNotificationThreadTarget::Thread(id) =
        server_notification_thread_target(notification)
    else {
        return;
    };
    match notification {
        ServerNotification::ThreadStarted(started)
            if !started.thread.ephemeral && !state.removed_threads.contains(&id) =>
        {
            let mut thread = started.thread.clone();
            normalize_thread(&mut thread);
            state.threads.insert(id, Some(thread));
        }
        ServerNotification::ThreadArchived(_) | ServerNotification::ThreadDeleted(_) => {
            state.threads.remove(&id);
            state.removed_threads.insert(id);
        }
        ServerNotification::ThreadStatusChanged(status) => {
            if let Some(Some(thread)) = state.threads.get_mut(&id) {
                thread.status = status.status.clone();
            }
        }
        ServerNotification::ThreadNameUpdated(name) => {
            if let Some(Some(thread)) = state.threads.get_mut(&id) {
                thread.name.clone_from(&name.thread_name);
            }
        }
        ServerNotification::ThreadSettingsUpdated(settings) => {
            if let Some(Some(thread)) = state.threads.get_mut(&id) {
                thread.cwd.clone_from(&settings.thread_settings.cwd);
                thread.model = Some(settings.thread_settings.model.clone());
                thread
                    .model_provider
                    .clone_from(&settings.thread_settings.model_provider);
            }
        }
        ServerNotification::ThreadClosed(_) => {
            if let Some(Some(thread)) = state.threads.get_mut(&id) {
                thread.status = codex_app_server_protocol::ThreadStatus::NotLoaded;
            }
        }
        ServerNotification::ItemCompleted(
            codex_app_server_protocol::ItemCompletedNotification {
                item: codex_app_server_protocol::ThreadItem::AgentMessage { text, .. },
                ..
            },
        ) => {
            state.last_messages.insert(
                id,
                super::super::agents_overview_details::preview_agent_message(text),
            );
        }
        ServerNotification::ThreadTokenUsageUpdated(usage) => {
            state.usage.entry(id).or_default().tokens = Some(usage.token_usage.total.clone());
        }
        _ => {}
    }
}

impl Drop for HostState {
    fn drop(&mut self) {
        // These handles belong to the UI; server turns and owned jobs remain on their hosts.
        for (_, (_, task)) in self.dynamic_tool_tasks.drain() {
            task.abort();
        }
        for (_, task) in self.thread_event_listener_tasks.drain() {
            task.abort();
        }
        for (_, cancellation) in self.pending_thread_titles.drain() {
            cancellation.cancel();
        }
        self.recap.stop_ui_tasks();
    }
}
