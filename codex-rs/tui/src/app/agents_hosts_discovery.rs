//! Lightweight metadata discovery and snapshot fencing for configured machines.

use super::agents_hosts::AgentHost;
use super::agents_hosts::ConnectionScope;
use super::agents_hosts::HostKey;
use super::agents_hosts::HostState;
use super::agents_hosts::HostUpdate;
use super::*;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;

impl App {
    pub(super) fn ensure_agents_hosts(&mut self, foreground: &AppServerSession) {
        if self.agents_hosts.started {
            return;
        }
        let Some(path) = self.agents_hosts.manifest.clone() else {
            return;
        };
        self.agents_hosts.started = true;
        self.agents_hosts.configuration_generation += 1;
        let generation = self.agents_hosts.configuration_generation;
        self.agents_hosts
            .hosts
            .entry(HostKey::default())
            .or_insert(AgentHost {
                label: if foreground.is_discovery_placeholder() {
                    "Overview"
                } else {
                    "Local"
                }
                .into(),
                machine_label: if foreground.is_discovery_placeholder() {
                    "Overview"
                } else {
                    "Local"
                }
                .into(),
                target: self.app_server_target.clone(),
                session: None,
                epoch: 0,
                connected: !foreground.is_discovery_placeholder(),
                state: HostState::default(),
                buffered: VecDeque::new(),
                revision: 0,
                refresh_generation: 0,
                refreshing: false,
                refresh_metadata: Vec::new(),
            });
        let foreground_target = self.app_server_target.clone();
        let executable = self
            .daemon_cli_executable
            .as_ref()
            .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
            .or_else(|| std::env::current_exe().ok());
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.notice = Some("Connecting configured machines…".into());
        self.agents_hosts.tasks.spawn(async move {
            let result = async {
                let manifest = codex_team::Manifest::load(&path).await?;
                let discovered = manifest
                    .machines
                    .iter()
                    .flat_map(|machine| {
                        machine.servers.iter().map(|server| {
                            (
                                HostKey {
                                    machine_id: machine.id.clone(),
                                    server_id: server.id.clone(),
                                },
                                format!("{} / {}", machine.label, server.label),
                                machine.label.clone(),
                            )
                        })
                    })
                    .collect();
                sender.send(AppEvent::AgentsHostUpdate(Box::new(
                    HostUpdate::Discovered {
                        generation,
                        hosts: discovered,
                    },
                )));
                let executable =
                    executable.ok_or_else(|| anyhow::anyhow!("Codex executable is unavailable"))?;
                let client = codex_team::ensure_coordinator(&path, &executable).await?;
                let mut hosts = Vec::new();
                let mut foreground = None;
                for machine in manifest.machines {
                    for server in machine.servers {
                        let key = HostKey {
                            machine_id: machine.id.clone(),
                            server_id: server.id,
                        };
                        let url = client.relay_url(&key.machine_id, &key.server_id)?;
                        let matches_foreground = match (&server.endpoint, &foreground_target) {
                            (
                                codex_team::Endpoint::Local { socket },
                                AppServerTarget::LocalDaemon {
                                    endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
                                    ..
                                },
                            ) => socket.as_path() == socket_path.as_path(),
                            (
                                codex_team::Endpoint::Websocket { url, .. },
                                AppServerTarget::Remote {
                                    endpoint:
                                        RemoteAppServerEndpoint::WebSocket { websocket_url, .. },
                                },
                            ) => url == websocket_url,
                            (
                                _,
                                AppServerTarget::Remote {
                                    endpoint:
                                        RemoteAppServerEndpoint::WebSocket { websocket_url, .. },
                                },
                            ) => url.as_str() == websocket_url,
                            _ => false,
                        };
                        if matches_foreground {
                            foreground = Some(key.clone());
                        }
                        let target = AppServerTarget::Remote {
                            endpoint: RemoteAppServerEndpoint::WebSocket {
                                websocket_url: url.to_string(),
                                auth_token: Some(client.relay_token().to_string()),
                            },
                        };
                        hosts.push((
                            key,
                            format!("{} / {}", machine.label, server.label),
                            machine.label.clone(),
                            target,
                        ));
                    }
                }
                Ok::<_, anyhow::Error>(HostUpdate::Configured {
                    generation,
                    client,
                    hosts,
                    foreground,
                })
            }
            .await;
            sender.send(AppEvent::AgentsHostUpdate(Box::new(result.unwrap_or_else(
                |_| HostUpdate::ConfigurationFailed {
                    generation,
                    error: "Could not load configured machines".into(),
                },
            ))));
        });
    }

    pub(super) fn connect_agents_host(&mut self, key: HostKey) {
        if self.agents_hosts.client.is_none() {
            self.agents_hosts.notice = Some("Connecting to the team coordinator…".into());
            return;
        }
        let Some(host) = self.agents_hosts.hosts.get_mut(&key) else {
            return;
        };
        host.epoch += 1;
        host.connected = false;
        host.refreshing = false;
        host.buffered.clear();
        host.refresh_metadata.clear();
        host.state.temporary_structured_requests.clear();
        for (_, cancellation) in host.state.pending_thread_titles.drain() {
            cancellation.cancel();
        }
        for (_, (_, task)) in host.state.dynamic_tool_tasks.drain() {
            task.abort();
        }
        host.state.dynamic_tool_origins.clear();
        let scope = ConnectionScope {
            host: key,
            epoch: host.epoch,
        };
        // Reconnecting replaces transport ownership without changing the host's logical identity.
        let backend_id = host.session.as_ref().map_or_else(
            crate::backend_id::BackendId::new,
            AppServerSession::backend_id,
        );
        let target = host.target.clone();
        let home = self.config.codex_home.clone();
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.tasks.spawn(async move {
            match crate::app_server_connection::connect(&target).await {
                Ok(client) => sender.send(AppEvent::AgentsHostUpdate(Box::new(
                    HostUpdate::Connected {
                        scope,
                        session: AppServerSession::new(
                            backend_id,
                            client,
                            target.thread_params_mode(),
                        )
                        .with_local_codex_home(&home),
                    },
                ))),
                Err(_) => sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Offline(
                    scope,
                )))),
            }
        });
    }

    pub(super) fn refresh_other_agents_hosts(&mut self, foreground: &AppServerSession) {
        let keys: Vec<_> = self.agents_hosts.hosts.keys().cloned().collect();
        self.refresh_agents_hosts(foreground, keys);
    }

    pub(super) fn refresh_agents_hosts(
        &mut self,
        foreground: &AppServerSession,
        keys: impl IntoIterator<Item = HostKey>,
    ) {
        for key in keys {
            if key == self.agents_hosts.foreground {
                continue;
            }
            let Some(handle) = self.agents_hosts.request_handle(&key, foreground) else {
                continue;
            };
            let Some(host) = self.agents_hosts.hosts.get_mut(&key) else {
                continue;
            };
            if !host.connected {
                continue;
            }
            if host.refreshing {
                host.state.agents_overview.refresh_pending = true;
                continue;
            }
            host.refreshing = true;
            host.refresh_metadata.clear();
            host.refresh_generation += 1;
            let generation = host.refresh_generation;
            let scope = ConnectionScope {
                host: key,
                epoch: host.epoch,
            };
            let revision = host.revision;
            let sender = self.app_event_tx.unscoped();
            self.agents_hosts.tasks.spawn(async move {
                // Read metadata only. Discovery never resumes or hydrates a thread.
                let result = async {
                    let mut threads = Vec::new();
                    let mut cursor = None;
                    loop {
                        let response: ThreadListResponse = handle.request_typed(ClientRequest::ThreadList {
                            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                            params: ThreadListParams {
                                cursor, limit: Some(100), model_providers: Some(Vec::new()),
                                archived: Some(false), source_kinds: Some(vec![
                                    codex_app_server_protocol::ThreadSourceKind::Cli,
                                    codex_app_server_protocol::ThreadSourceKind::VsCode,
                                    codex_app_server_protocol::ThreadSourceKind::Exec,
                                    codex_app_server_protocol::ThreadSourceKind::AppServer,
                                    codex_app_server_protocol::ThreadSourceKind::SubAgentThreadSpawn,
                                    codex_app_server_protocol::ThreadSourceKind::Unknown,
                                ]),
                                excluded_thread_ids: None, originators: None, sort_key: None, sort_direction: None,
                                section_id: None, project_id: None, parent_thread_id: None, ancestor_thread_id: None,
                                cwd: None, use_state_db_only: false, search_term: None,
                            },
                        }).await.map_err(|_| "Machine thread listing failed".to_string())?;
                        threads.extend(response.data.into_iter().filter(|thread| !thread.ephemeral));
                        let Some(next) = response.next_cursor else { break };
                        cursor = Some(next);
                    }

                    // Loaded Board recipients can lack the user preview required by thread/list.
                    let mut seen: HashSet<_> = threads.iter().map(|thread| thread.id.clone()).collect();
                    let mut cursor = None;
                    loop {
                        let response = handle.request_typed::<ThreadLoadedListResponse>(ClientRequest::ThreadLoadedList {
                            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                            params: ThreadLoadedListParams { cursor, limit: Some(100) },
                        }).await;
                        let response = match response {
                            Ok(response) => response,
                            Err(error) => {
                                tracing::warn!(%error, "failed to list loaded machine threads");
                                break;
                            }
                        };
                        for thread_id in response.data {
                            if !seen.insert(thread_id.clone()) {
                                continue;
                            }
                            match handle.request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                                request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                                params: ThreadReadParams { thread_id: thread_id.clone(), include_turns: false },
                            }).await {
                                Ok(response) if !response.thread.ephemeral => threads.push(response.thread),
                                Ok(_) => {},
                                // A loaded thread may disappear between listing and metadata lookup.
                                Err(error) => tracing::warn!(%thread_id, %error, "failed to read loaded machine thread"),
                            }
                        }
                        let Some(next) = response.next_cursor else { break };
                        cursor = Some(next);
                    }
                    Ok(threads)
                }.await;
                sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Listed { scope: scope.clone(), generation, revision, result })));
                if let Ok(pins) = super::agents_overview_discovery::list_pinned_threads(&handle).await {
                    sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::PinsListed {
                        scope, generation, pins: pins.map(|threads| threads.iter().filter_map(|thread| ThreadId::from_string(&thread.id).ok()).collect()),
                    })));
                }
            });
        }
    }

    pub(super) fn apply_agents_host_pins(
        &mut self,
        scope: ConnectionScope,
        generation: u64,
        pins: Option<Vec<ThreadId>>,
    ) {
        if !self.agents_hosts.accepts(&scope) {
            return;
        }
        #[expect(
            clippy::expect_used,
            reason = "The scope was accepted immediately above, which checks this host entry and epoch."
        )]
        let host = self
            .agents_hosts
            .hosts
            .get_mut(&scope.host)
            .expect("host exists");
        if generation != host.refresh_generation {
            return;
        }
        let overview = if scope.host == self.agents_hosts.foreground {
            &mut self.agents_overview
        } else {
            &mut host.state.agents_overview
        };
        overview.pinned_thread_ids = pins;
    }

    pub(super) fn apply_agents_host_listing(
        &mut self,
        scope: ConnectionScope,
        generation: u64,
        revision: u64,
        result: Result<Vec<codex_app_server_protocol::Thread>, String>,
    ) -> bool {
        if !self.agents_hosts.accepts(&scope) {
            return false;
        }
        #[expect(
            clippy::expect_used,
            reason = "The scope was accepted immediately above, which checks this host entry and epoch."
        )]
        let host = self
            .agents_hosts
            .hosts
            .get_mut(&scope.host)
            .expect("host exists");
        if generation != host.refresh_generation {
            return false;
        }
        host.refreshing = false;
        let overview = if scope.host == self.agents_hosts.foreground {
            &mut self.agents_overview
        } else {
            &mut host.state.agents_overview
        };
        match result {
            Ok(threads) => {
                for mut thread in threads {
                    let Ok(id) = ThreadId::from_string(&thread.id) else {
                        continue;
                    };
                    super::agents_hosts::state::normalize_thread(&mut thread);
                    // Lifecycle tombstones and newer notifications win over an earlier snapshot.
                    if !overview.removed_threads.contains(&id)
                        && (host.revision == revision
                            || overview.threads.get(&id).is_none_or(Option::is_none))
                    {
                        overview.threads.insert(id, Some(thread));
                    }
                }
            }
            Err(error) => self.agents_hosts.notice = Some(error),
        }
        // Ordered notifications observed after the snapshot began also cover absent records.
        for (observed_revision, notification) in std::mem::take(&mut host.refresh_metadata) {
            if observed_revision > revision {
                super::agents_hosts::state::track_notification(overview, &notification);
            }
        }
        std::mem::take(&mut overview.refresh_pending)
    }
}
