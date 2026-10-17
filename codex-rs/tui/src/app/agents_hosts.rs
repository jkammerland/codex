//! Machine-owned connections for the existing agents overview and conversation loop.
//!
//! Discovery only initializes transports and lists metadata. Preparing an attachment is
//! separate from committing foreground state, so a slow or unavailable machine cannot
//! take away the conversation currently displayed by the user.

use super::*;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_team::TeamClient;
use codex_team::TeamSnapshot;
use std::collections::BTreeMap;

#[path = "agents_hosts_state.rs"]
pub(super) mod state;
pub(super) use state::HostState;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct HostKey {
    pub(crate) machine_id: String,
    pub(crate) server_id: String,
}

impl Default for HostKey {
    fn default() -> Self {
        Self {
            machine_id: "@local".into(),
            server_id: "@current".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ThreadTarget {
    pub(crate) host: HostKey,
    pub(crate) thread_id: ThreadId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionScope {
    pub(crate) host: HostKey,
    pub(crate) epoch: u64,
}

pub(super) enum HostBufferedEvent {
    Event(AppServerEvent),
    Request(Box<super::app_server_requests::BufferedServerRequest>),
}

pub(super) struct AgentHost {
    pub(super) label: String,
    pub(super) machine_label: String,
    pub(super) target: AppServerTarget,
    pub(super) session: Option<AppServerSession>,
    pub(super) epoch: u64,
    pub(super) connected: bool,
    pub(super) state: HostState,
    pub(super) buffered: VecDeque<HostBufferedEvent>,
    pub(super) revision: u64,
    pub(super) refresh_generation: u64,
    pub(super) refreshing: bool,
    pub(super) refresh_metadata: Vec<(u64, ServerNotification)>,
}

pub(super) struct AgentsHosts {
    pub(super) foreground: HostKey,
    pub(super) hosts: BTreeMap<HostKey, AgentHost>,
    pub(super) generation: u64,
    pub(super) prepare_generation: u64,
    pub(super) initial_host: HostKey,
    pub(super) manifest: Option<PathBuf>,
    pub(super) client: Option<TeamClient>,
    pub(super) snapshot: Option<TeamSnapshot>,
    pub(super) board:
        Arc<std::sync::Mutex<super::agents_overview_view::command_center::board::BoardState>>,
    pub(super) team_read_generation: Arc<std::sync::atomic::AtomicU64>,
    pub(super) applied_team_read: u64,
    pub(super) notice: Option<String>,
    pub(super) team_operations: BTreeMap<String, codex_team::TeamOperation>,
    pub(super) failed_team_operations: std::collections::BTreeSet<String>,
    pub(super) started: bool,
    pub(super) configuration_generation: u64,
    pub(super) tasks: tokio::task::JoinSet<()>,
}

impl Default for AgentsHosts {
    fn default() -> Self {
        Self {
            foreground: HostKey::default(),
            hosts: BTreeMap::new(),
            generation: 0,
            prepare_generation: 0,
            initial_host: HostKey::default(),
            manifest: None,
            client: None,
            snapshot: None,
            board: Arc::default(),
            notice: None,
            started: false,
            configuration_generation: 0,
            team_read_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            applied_team_read: 0,
            tasks: tokio::task::JoinSet::new(),
            team_operations: BTreeMap::new(),
            failed_team_operations: std::collections::BTreeSet::new(),
        }
    }
}

pub(crate) enum TeamOperationFailure {
    Rejected(String),
    Unknown(String),
}

pub(crate) enum HostUpdate {
    ClosedVerified {
        scope: ConnectionScope,
        generation: u64,
        revision: u64,
        notification: ServerNotification,
        confirmed: bool,
    },
    PromptRevertPrepared {
        scope: ConnectionScope,
        request_id: Uuid,
        result: Result<
            super::agents_hosts_revert_preparation::PreparedPromptRevert,
            super::agents_hosts_revert_preparation::PromptRevertFailure,
        >,
    },
    ConfigurationFailed {
        generation: u64,
        error: String,
    },
    Discovered {
        generation: u64,
        hosts: Vec<(HostKey, String, String)>,
    },
    Configured {
        generation: u64,
        client: TeamClient,
        hosts: Vec<(HostKey, String, String, AppServerTarget)>,
        foreground: Option<HostKey>,
    },
    Connected {
        scope: ConnectionScope,
        session: AppServerSession,
    },
    Event {
        scope: ConnectionScope,
        event: AppServerEvent,
    },
    PinsListed {
        scope: ConnectionScope,
        pins: Option<Vec<ThreadId>>,
        generation: u64,
    },
    UsageLoaded {
        scope: ConnectionScope,
        thread_id: ThreadId,
        request_id: Uuid,
        result: Result<crate::chatwidget::ThreadUsageOutcome, String>,
    },
    Listed {
        scope: ConnectionScope,
        generation: u64,
        revision: u64,
        result: Result<Vec<codex_app_server_protocol::Thread>, String>,
    },
    Prepared {
        scope: ConnectionScope,
        generation: u64,
        result: Result<Box<PreparedHost>, String>,
        session: AppServerSession,
    },
    TeamSnapshot {
        generation: u64,
        snapshot: TeamSnapshot,
    },
    Error(String),
    Offline(ConnectionScope),
    ActionFinished {
        scope: ConnectionScope,
        result: Result<(), String>,
    },
    LifecycleFinished {
        scope: ConnectionScope,
        thread_id: ThreadId,
        result: Result<(), String>,
    },
    TeamOperationFinished {
        operation_id: String,
        result: Result<(), TeamOperationFailure>,
    },
}

impl std::fmt::Debug for HostUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HostUpdate")
    }
}

pub(crate) struct PreparedHost {
    pub(super) config: Config,
    pub(super) bootstrap: AppServerBootstrap,
    pub(super) started: AppServerStartedThread,
    pub(super) read_only: bool,
}

impl AgentsHosts {
    pub(super) fn note_foreground_event(&mut self) {
        if let Some(host) = self.hosts.get_mut(&self.foreground) {
            host.revision += 1;
        }
    }

    pub(super) fn spawn(&mut self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(future);
    }

    pub(super) fn enabled(&self) -> bool {
        self.manifest.is_some()
    }

    pub(super) fn scope(&self) -> ConnectionScope {
        ConnectionScope {
            host: self.foreground.clone(),
            epoch: self
                .hosts
                .get(&self.foreground)
                .map_or(0, |host| host.epoch),
        }
    }

    pub(super) fn canonical_host(&self, host: &HostKey) -> HostKey {
        if host == &HostKey::default() {
            self.initial_host.clone()
        } else {
            host.clone()
        }
    }

    pub(super) fn accepts(&self, scope: &ConnectionScope) -> bool {
        self.hosts
            .get(&self.canonical_host(&scope.host))
            .is_some_and(|host| host.epoch == scope.epoch)
    }

    pub(super) fn request_handle(
        &self,
        key: &HostKey,
        foreground: &AppServerSession,
    ) -> Option<AppServerRequestHandle> {
        let key = self.canonical_host(key);
        if key == self.foreground {
            foreground.try_request_handle()
        } else {
            self.hosts
                .get(&key)?
                .session
                .as_ref()
                .and_then(AppServerSession::try_request_handle)
        }
    }

    pub(super) fn route(
        &mut self,
        scope: ConnectionScope,
        session: &mut AppServerSession,
        sender: AppEventSender,
    ) {
        let Some(mut events) = session.take_events() else {
            return;
        };
        self.tasks.spawn(async move {
            while let Some(event) = events.recv().await {
                sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Event {
                    scope: scope.clone(),
                    event,
                })));
            }
            sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Offline(
                scope,
            ))));
        });
    }
}

impl App {
    pub(super) async fn handle_agents_host_update(
        &mut self,
        tui: &mut tui::Tui,
        foreground: &mut AppServerSession,
        mut update: HostUpdate,
    ) -> Result<()> {
        let scope = match &mut update {
            HostUpdate::Connected { scope, .. }
            | HostUpdate::Event { scope, .. }
            | HostUpdate::ClosedVerified { scope, .. }
            | HostUpdate::PromptRevertPrepared { scope, .. }
            | HostUpdate::Listed { scope, .. }
            | HostUpdate::PinsListed { scope, .. }
            | HostUpdate::UsageLoaded { scope, .. }
            | HostUpdate::Prepared { scope, .. }
            | HostUpdate::ActionFinished { scope, .. }
            | HostUpdate::LifecycleFinished { scope, .. }
            | HostUpdate::Offline(scope) => Some(scope),
            HostUpdate::Configured { .. }
            | HostUpdate::Discovered { .. }
            | HostUpdate::ConfigurationFailed { .. }
            | HostUpdate::TeamSnapshot { .. }
            | HostUpdate::Error(_)
            | HostUpdate::TeamOperationFinished { .. } => None,
        };
        if let Some(scope) = scope {
            scope.host = self.agents_hosts.canonical_host(&scope.host);
        }
        while self.agents_hosts.tasks.try_join_next().is_some() {}
        match update {
            HostUpdate::PromptRevertPrepared {
                scope,
                request_id,
                result,
            } => {
                self.finish_host_prompt_revert_preparation(scope, request_id, result)
                    .await?;
            }
            HostUpdate::ConfigurationFailed { generation, error } => {
                if self.agents_hosts.configuration_generation == generation
                    && self.agents_hosts.client.is_none()
                {
                    self.agents_hosts.started = false;
                    self.agents_hosts.notice = Some(error);
                }
            }
            HostUpdate::Discovered { generation, hosts } => {
                if generation != self.agents_hosts.configuration_generation {
                    return Ok(());
                }
                for (key, label, machine_label) in hosts {
                    self.agents_hosts.hosts.entry(key).or_insert(AgentHost {
                        label,
                        machine_label,
                        target: AppServerTarget::Embedded,
                        session: None,
                        epoch: 0,
                        connected: false,
                        state: HostState::default(),
                        buffered: VecDeque::new(),
                        revision: 0,
                        refresh_generation: 0,
                        refreshing: false,
                        refresh_metadata: Vec::new(),
                    });
                }
            }
            HostUpdate::ClosedVerified {
                scope,
                generation,
                revision,
                notification,
                confirmed,
            } => {
                if confirmed
                    && self.agents_hosts.accepts(&scope)
                    && scope.host == self.agents_hosts.foreground
                    && generation == self.agents_hosts.generation
                    && self
                        .agents_hosts
                        .hosts
                        .get(&scope.host)
                        .is_some_and(|host| host.revision == revision)
                {
                    self.handle_server_notification_event(foreground, notification, false)
                        .await;
                }
            }
            HostUpdate::Configured {
                generation,
                client,
                hosts,
                foreground: configured_foreground,
            } => {
                if generation != self.agents_hosts.configuration_generation {
                    return Ok(());
                }
                if let Some(key) = configured_foreground
                    && let Some(host) = self
                        .agents_hosts
                        .hosts
                        .remove(&self.agents_hosts.foreground)
                {
                    foreground.scope_task_tool_cache(
                        &self.config.codex_home,
                        &key.machine_id,
                        &key.server_id,
                    );
                    self.agents_hosts.initial_host = key.clone();
                    self.agents_hosts.foreground = key.clone();
                    self.agents_hosts.hosts.insert(key, host);
                }
                self.agents_hosts.client = Some(client.clone());

                // Coordinator discovery is complete; retain any newer operation error.
                if matches!(
                    self.agents_hosts.notice.as_deref(),
                    Some("Connecting configured machines…" | "Connecting to the team coordinator…")
                ) {
                    self.agents_hosts.notice = None;
                }

                for (key, label, machine_label, target) in hosts {
                    if let Some(host) = self.agents_hosts.hosts.get_mut(&key) {
                        host.label = label;
                        host.machine_label = machine_label;
                        host.target = target;
                        if key != self.agents_hosts.foreground && host.session.is_none() {
                            self.connect_agents_host(key);
                        }
                        continue;
                    }
                    self.agents_hosts.hosts.insert(
                        key.clone(),
                        AgentHost {
                            label,
                            machine_label,
                            target,
                            session: None,
                            epoch: 0,
                            connected: false,
                            state: HostState::default(),
                            buffered: VecDeque::new(),
                            revision: 0,
                            refresh_generation: 0,
                            refreshing: false,
                            refresh_metadata: Vec::new(),
                        },
                    );
                    self.connect_agents_host(key);
                }
                let sender = self.app_event_tx.unscoped();
                let generations = self.agents_hosts.team_read_generation.clone();
                self.agents_hosts.tasks.spawn(async move {
                    let Ok(mut events) = client.events().await else {
                        sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Error(
                            "Team coordinator is unavailable".into(),
                        ))));
                        return;
                    };
                    loop {
                        let generation =
                            generations.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if let Ok(snapshot) = client.snapshot().await {
                            sender.send(AppEvent::AgentsHostUpdate(Box::new(
                                HostUpdate::TeamSnapshot {
                                    generation,
                                    snapshot,
                                },
                            )));
                        }
                        if events.changed().await.is_err() {
                            break;
                        }
                    }
                });
            }
            HostUpdate::Connected { scope, mut session } => {
                if !self.agents_hosts.accepts(&scope) {
                    return Ok(());
                }
                session.scope_task_tool_cache(
                    &self.config.codex_home,
                    &scope.host.machine_id,
                    &scope.host.server_id,
                );
                self.agents_hosts
                    .route(scope.clone(), &mut session, self.app_event_tx.unscoped());
                #[expect(
                    clippy::expect_used,
                    reason = "The accepted scope guarantees this host entry; routing does not remove hosts."
                )]
                let host = self
                    .agents_hosts
                    .hosts
                    .get_mut(&scope.host)
                    .expect("host exists");
                host.session = Some(session);
                host.connected = true;
                self.refresh_other_agents_hosts(foreground);
            }
            HostUpdate::UsageLoaded {
                scope,
                thread_id,
                request_id,
                result,
            } => {
                if !self.agents_hosts.accepts(&scope) {
                    return Ok(());
                }
                if scope.host == self.agents_hosts.foreground {
                    super::agents_overview_usage::apply_usage_to_state(
                        &mut self.agents_overview,
                        thread_id,
                        request_id,
                        result,
                    );
                } else if let Some(host) = self.agents_hosts.hosts.get_mut(&scope.host) {
                    super::agents_overview_usage::apply_usage_to_state(
                        &mut host.state.agents_overview,
                        thread_id,
                        request_id,
                        result,
                    );
                }
            }
            HostUpdate::PinsListed {
                scope,
                pins,
                generation,
            } => {
                self.apply_agents_host_pins(scope, generation, pins);
            }
            HostUpdate::Listed {
                scope,
                generation,
                revision,
                result,
            } => {
                let host = scope.host.clone();
                if self.apply_agents_host_listing(scope, generation, revision, result) {
                    self.refresh_agents_hosts(foreground, [host]);
                }
            }
            HostUpdate::Event { scope, event } => {
                if !self.agents_hosts.accepts(&scope) {
                    return Ok(());
                }
                if let AppServerEvent::ServerNotification(notification) = &event
                    && matches!(
                        notification.as_ref(),
                        ServerNotification::ThreadStarted(_)
                            | ServerNotification::ThreadArchived(_)
                            | ServerNotification::ThreadDeleted(_)
                            | ServerNotification::ThreadStatusChanged(_)
                            | ServerNotification::ThreadNameUpdated(_)
                            | ServerNotification::ThreadSettingsUpdated(_)
                            | ServerNotification::ThreadClosed(_)
                    )
                    && let Some(host) = self.agents_hosts.hosts.get_mut(&scope.host)
                    && host.refreshing
                {
                    host.refresh_metadata
                        .push((host.revision + 1, notification.as_ref().clone()));
                }
                if scope.host == self.agents_hosts.foreground {
                    self.handle_app_server_event(foreground, event).await;
                } else {
                    let origin = self.agents_hosts.request_handle(&scope.host, foreground);
                    #[expect(
                        clippy::expect_used,
                        reason = "The scope was accepted before this branch, with no intervening host removal."
                    )]
                    let host = self
                        .agents_hosts
                        .hosts
                        .get_mut(&scope.host)
                        .expect("host exists");
                    let discover_loaded_thread = matches!(
                        &event,
                        AppServerEvent::ServerNotification(notification)
                            if matches!(notification.as_ref(), ServerNotification::ThreadStatusChanged(status)
                                if status.status != codex_app_server_protocol::ThreadStatus::NotLoaded
                                    && ThreadId::from_string(&status.thread_id).is_ok_and(|id| {
                                        !host.state.agents_overview.removed_threads.contains(&id)
                                            && host.state.agents_overview.threads.get(&id).is_none_or(Option::is_none)
                                    }))
                    );
                    host.revision += 1;
                    if let AppServerEvent::ServerNotification(notification) = &event {
                        if let ServerNotification::ThreadStatusChanged(status) =
                            notification.as_ref()
                            && let Some(updates) = &host.state.dynamic_tool_status_updates
                        {
                            let _ = updates.send(status.clone());
                        }
                        if let ServerNotification::ServerRequestResolved(resolved) =
                            notification.as_ref()
                        {
                            host.state.dynamic_tool_origins.remove(&resolved.request_id);
                            if let Some((_, task)) =
                                host.state.dynamic_tool_tasks.remove(&resolved.request_id)
                            {
                                task.abort();
                            }
                            host.state
                                .pending_app_server_requests
                                .resolve_notification(&resolved.thread_id, &resolved.request_id);
                            host.buffered.retain(|event| !matches!(event, HostBufferedEvent::Request(request) if request.request.id() == &resolved.request_id));
                        }
                        if let super::app_server_event_targets::ServerNotificationThreadTarget::Thread(id) = super::app_server_event_targets::server_notification_thread_target(notification)
                            && let Some(sender) = host.state.temporary_structured_requests.get(&id)
                        {
                            if matches!(notification.as_ref(), ServerNotification::ItemCompleted(_) | ServerNotification::TurnCompleted(_)) {
                                let _ = sender.send(notification.as_ref().clone());
                            }
                            return Ok(());
                        }
                        state::track_notification(&mut host.state.agents_overview, notification);
                    }
                    match event {
                        AppServerEvent::ServerRequest(request) => {
                            if let Some(origin) = origin {
                                if super::app_server_event_targets::server_request_thread_id(
                                    &request,
                                )
                                .is_some_and(|id| {
                                    host.state.temporary_structured_requests.contains_key(&id)
                                }) {
                                    let reply =
                                        origin.server_request_reply_handle(request.id().clone());
                                    self.agents_hosts.spawn(async move {
                                        let _ = reply.reject(codex_app_server_protocol::JSONRPCErrorError {
                                            code: -32000,
                                            message: "temporary structured threads cannot request tools or user interaction".into(),
                                            data: None,
                                        }).await;
                                    });
                                    return Ok(());
                                }
                                host.buffered.push_back(HostBufferedEvent::Request(Box::new(
                                    super::app_server_requests::BufferedServerRequest::new(
                                        *request, &origin,
                                    ),
                                )));
                            }
                        }
                        event => host.buffered.push_back(HostBufferedEvent::Event(event)),
                    }

                    // A coordinator resume can become loaded after the discovery snapshot.
                    if discover_loaded_thread {
                        self.refresh_agents_hosts(foreground, [scope.host]);
                    }
                }
            }
            HostUpdate::Prepared {
                scope,
                generation,
                result,
                session,
            } => {
                self.commit_agents_host(tui, foreground, scope, generation, result, session)
                    .await?;
            }
            HostUpdate::LifecycleFinished {
                scope,
                thread_id,
                result,
            } => {
                self.finish_host_lifecycle(tui, foreground, scope, thread_id, result)
                    .await?;
            }
            HostUpdate::ActionFinished { scope, result } => {
                if self.agents_hosts.accepts(&scope) {
                    if let Err(error) = result {
                        self.agents_hosts.notice = Some(error);
                    }
                    self.refresh_other_agents_hosts(foreground);
                }
            }
            HostUpdate::TeamOperationFinished {
                operation_id,
                result,
            } => match result {
                Err(TeamOperationFailure::Rejected(reason)) => {
                    let retain_read_retry = self
                        .agents_hosts
                        .board
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .operation_rejected(&operation_id, &reason);
                    if retain_read_retry {
                        self.agents_hosts
                            .failed_team_operations
                            .insert(operation_id);
                    } else {
                        self.agents_hosts.team_operations.remove(&operation_id);
                        self.agents_hosts
                            .failed_team_operations
                            .remove(&operation_id);
                    }
                    self.agents_hosts.notice = Some(reason);
                }
                result @ (Ok(()) | Err(TeamOperationFailure::Unknown(_))) => {
                    let confirmed = self
                        .agents_hosts
                        .board
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .operation_finished(&operation_id, result.is_ok());
                    if confirmed {
                        self.agents_hosts.team_operations.remove(&operation_id);
                        self.agents_hosts
                            .failed_team_operations
                            .remove(&operation_id);
                    } else if let Err(TeamOperationFailure::Unknown(error)) = result {
                        self.agents_hosts
                            .failed_team_operations
                            .insert(operation_id);
                        self.agents_hosts.notice = Some(error);
                    }
                }
            },
            HostUpdate::TeamSnapshot {
                generation,
                snapshot,
            } => {
                if generation >= self.agents_hosts.applied_team_read {
                    self.agents_hosts.applied_team_read = generation;
                    let settled = {
                        let mut board = self
                            .agents_hosts
                            .board
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        board.update(snapshot.board.clone());
                        board.take_confirmed_failed_reads()
                    };
                    for id in settled {
                        self.agents_hosts.team_operations.remove(&id);
                        self.agents_hosts.failed_team_operations.remove(&id);
                    }
                    self.agents_hosts.snapshot = Some(snapshot);
                }
            }
            HostUpdate::Error(error) => self.agents_hosts.notice = Some(error),
            HostUpdate::Offline(scope) => {
                if !self.agents_hosts.accepts(&scope) {
                    return Ok(());
                }
                #[expect(
                    clippy::expect_used,
                    reason = "The accepted scope guarantees this host entry until reconnection begins below."
                )]
                let host = self
                    .agents_hosts
                    .hosts
                    .get_mut(&scope.host)
                    .expect("host exists");
                let was_connected = host.connected;
                host.connected = false;
                if scope.host == self.agents_hosts.foreground {
                    self.begin_reconnect();
                } else if was_connected {
                    // A genuine transport loss starts one new connection. Failures remain visible
                    // for an explicit user retry rather than keeping a reconnect timer alive.
                    self.connect_agents_host(scope.host);
                }
            }
        }
        self.repaint_agents_overview();
        tui.frame_requester().schedule_frame();
        Ok(())
    }
}

#[cfg(test)]
#[path = "agents_hosts_tests.rs"]
mod tests;
