use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::coordinator::status_text;
use crate::model::*;
use crate::thread_metadata::ThreadReadMetadata;

impl Coordinator {
    pub(crate) async fn reconcile(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        let key = server_key(&machine.id, &server.id);
        let host: TeamHostReadResponse = peer
            .handle
            .request_typed(ClientRequest::TeamHostRead {
                request_id: request_id(),
                params: TeamHostReadParams {
                    cursor: None,
                    limit: Some(1000),
                },
            })
            .await?;
        ensure!(
            host.protocol_version == 4,
            "worker team protocol {} is incompatible with coordinator protocol 4; install matching coordinator and worker builds",
            host.protocol_version
        );
        if let Some(expected) = self.store.get::<String>("installation", &key).await? {
            ensure!(
                expected == host.installation_id,
                "worker installation changed; verify the machine, account and manifest before re-enrolling"
            );
        } else {
            self.store
                .put("installation", &key, &host.installation_id)
                .await?;
        }
        let current = self.current_control().await?;
        if let Some(applied) = &host.control {
            ensure!(
                applied.coordinator_id == current.coordinator_id,
                "worker belongs to another coordinator"
            );
            ensure!(
                applied.revision <= current.revision,
                "worker control is newer than coordinator storage; restore the coordinator database"
            );
        }
        peer.verified
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(status) = self.statuses.write().await.get_mut(&key) {
            status.installation_id = Some(host.installation_id);
            status.applied_revision = host.control.as_ref().map(|control| control.revision);
        }
        self.notify();
        // Reapply the matching revision once to release the startup
        // reconciliation gate. Missing revisions, including Stop, stay ordered.
        self.controls(machine, server, peer, host.control, true)
            .await?;
        self.refresh_enrollments(machine, server, peer, true)
            .await?;
        self.synchronize(machine, server, peer).await?;
        if let Some(status) = self.statuses.write().await.get_mut(&key) {
            status.connected = true;
            status.error = None;
        }
        Ok(())
    }

    pub(crate) async fn dispatch_controls(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        if !peer.verified.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        let status = self
            .statuses
            .read()
            .await
            .get(&server_key(&machine.id, &server.id))
            .cloned();
        let Some(status) = status.filter(|status| status.installation_id.is_some()) else {
            return Ok(());
        };
        let coordinator_id = self.current_control().await?.coordinator_id;
        let applied = status.applied_revision.map(|revision| TeamControlState {
            coordinator_id,
            revision,
            mode: TeamExecutionMode::Running,
        });
        self.controls(machine, server, peer, applied, false).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the destination control lane must send and acknowledge durable revisions in order across RPCs"
    )]
    async fn controls(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
        applied: Option<TeamControlState>,
        reconcile: bool,
    ) -> Result<()> {
        let lock = self.control_update_lock(&machine.id, &server.id).await;
        let _control = lock.lock().await;
        let current = self.current_control().await?;
        let applied = if !reconcile {
            let cached = self
                .statuses
                .read()
                .await
                .get(&server_key(&machine.id, &server.id))
                .and_then(|status| status.applied_revision);
            cached
                .filter(|revision| {
                    applied
                        .as_ref()
                        .is_none_or(|value| *revision >= value.revision)
                })
                .map(|revision| TeamControlState {
                    coordinator_id: current.coordinator_id.clone(),
                    revision,
                    mode: current.mode,
                })
                .or(applied)
        } else {
            applied
        };
        if let Some(applied) = &applied {
            ensure!(
                applied.coordinator_id == current.coordinator_id,
                "worker belongs to another coordinator"
            );
            ensure!(
                applied.revision <= current.revision,
                "worker control is newer than coordinator storage; restore the coordinator database"
            );
        }
        let after = applied
            .as_ref()
            .map_or(String::new(), |control| format!("{:020}", control.revision));
        let mut revisions = self
            .store
            .list::<TeamControlState>("control_history", &after, u32::MAX)
            .await?
            .into_iter()
            .map(|(_, control)| control)
            .collect::<Vec<_>>();
        if reconcile {
            if let Some(stopped) =
                applied.filter(|control| control.mode == TeamExecutionMode::Stopped)
            {
                // Stop is persisted before interruption and cancellation. A
                // crash can leave those actions unfinished even when a later
                // Resume is already queued at the coordinator.
                revisions.insert(0, stopped);
            } else if revisions.is_empty() {
                revisions.push(current);
            }
        }
        for control in revisions {
            let requested_revision = control.revision;
            let response: TeamControlResponse = peer
                .handle
                .request_typed(ClientRequest::TeamControl {
                    request_id: request_id(),
                    params: TeamControlParams { control },
                })
                .await?;
            let acknowledged = self
                .store
                .get::<TeamControlState>(
                    "control_history",
                    &format!("{:020}", response.control.revision),
                )
                .await?;
            ensure!(
                response.control.revision >= requested_revision
                    && acknowledged.as_ref() == Some(&response.control),
                "worker acknowledged an inconsistent control revision"
            );
            // The returned control acknowledges durable admission. Cleanup can
            // remain pending while later ordered controls use this healthy peer.
            if let Some(status) = self
                .statuses
                .write()
                .await
                .get_mut(&server_key(&machine.id, &server.id))
            {
                status.applied_revision = Some(response.control.revision);
            }
            self.notify();
        }
        Ok(())
    }

    pub(crate) async fn synchronize(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        let host: TeamHostReadResponse = peer
            .handle
            .request_typed(ClientRequest::TeamHostRead {
                request_id: request_id(),
                params: TeamHostReadParams {
                    cursor: None,
                    limit: Some(1),
                },
            })
            .await?;
        self.controls(machine, server, peer, host.control, false)
            .await?;
        self.refresh_enrollments(machine, server, peer, false)
            .await?;
        self.inherit_models(machine, server, peer).await?;
        self.drain_outbox(machine, server, peer).await?;
        self.publish().await?;
        {
            let board_state = self.board_snapshot().await?;
            let synchronized_board = board_state
                .entries
                .iter()
                .map(|entry| (entry.message.id.clone(), entry.append_seq))
                .collect::<std::collections::HashMap<_, _>>();
            let board = board_state
                .entries
                .iter()
                .map(|entry| entry.message.clone())
                .collect();
            let mut message_status = self.message_status_snapshot().await?;
            message_status.data.retain(|status| {
                (status.sender.machine_id == machine.id && status.sender.server_id == server.id)
                    || status.recipients.iter().any(|recipient| {
                        recipient.address.machine_id == machine.id
                            && recipient.address.server_id == server.id
                    })
            });
            let _: TeamSyncResponse = peer
                .handle
                .request_typed(ClientRequest::TeamSync {
                    request_id: request_id(),
                    params: TeamSyncParams {
                        coordinator_id: self.current_control().await?.coordinator_id,
                        directory: self.directory_snapshot().await?,
                        board,
                        board_state,
                        cancellation_adapters: server.cancellation_adapters.clone(),
                        message_status,
                    },
                })
                .await?;
            if self.current_control().await?.mode == TeamExecutionMode::Running {
                self.deliver_to(&machine.id, &server.id, peer, &synchronized_board)
                    .await?;
            }
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "snapshot version checks and durable snapshot commits share a mutation fence with newer notifications"
    )]
    async fn refresh_enrollments(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
        rejoin: bool,
    ) -> Result<()> {
        let mut cursor = None;
        let mut present = std::collections::HashSet::new();
        let initial_version = self
            .observed_versions
            .lock()
            .await
            .get(&server_key(&machine.id, &server.id))
            .copied()
            .unwrap_or_default();
        loop {
            let version = self
                .observed_versions
                .lock()
                .await
                .get(&server_key(&machine.id, &server.id))
                .copied()
                .unwrap_or_default();
            let host: TeamHostReadResponse = peer
                .handle
                .request_typed(ClientRequest::TeamHostRead {
                    request_id: request_id(),
                    params: TeamHostReadParams {
                        cursor,
                        limit: Some(1000),
                    },
                })
                .await?;
            for mut enrollment in host.data {
                enrollment.address = canonical_address(enrollment.address)?;
                ensure!(
                    enrollment.address.machine_id == machine.id
                        && enrollment.address.server_id == server.id,
                    "manifest identity differs from worker enrollment"
                );
                let key = address_key(&enrollment.address);
                present.insert(key.clone());
                let previous = self.store.get::<Agent>("agent", &key).await?;
                let mut agent = previous.clone().unwrap_or_else(|| Agent {
                    discovery: Default::default(),
                    enrollment: enrollment.clone(),
                    requested_selection: None,
                    runtime: None,
                    status: "unloaded".into(),
                    current_model: None,
                    current_effort: None,
                    next_model: None,
                    next_effort: None,
                    project: None,
                });
                agent.enrollment = enrollment;
                // Read stored metadata without loading an idle or archived teammate.
                if rejoin || previous.is_none() {
                    let read: ThreadReadMetadata = peer
                        .handle
                        .request_typed(ClientRequest::ThreadRead {
                            request_id: request_id(),
                            params: ThreadReadParams {
                                thread_id: agent.enrollment.address.thread_id.clone(),
                                include_turns: false,
                            },
                        })
                        .await?;
                    agent.status = status_text(&read.thread.status);
                    agent.project = read.thread.project_id;
                }
                let _mutation = self.mutations.lock().await;
                if self
                    .observed_versions
                    .lock()
                    .await
                    .get(&server_key(&machine.id, &server.id))
                    .copied()
                    .unwrap_or_default()
                    != version
                {
                    continue;
                }
                if previous.as_ref() != Some(&agent) {
                    self.store.put("agent", &key, &agent).await?;
                    self.notify();
                }
            }
            for runtime in host.runtime {
                let address = canonical_address(TeamAddress {
                    machine_id: machine.id.clone(),
                    server_id: server.id.clone(),
                    thread_id: runtime.thread_id.clone(),
                })?;
                let key = address_key(&address);
                present.insert(key.clone());
                let previous = self.store.get::<Agent>("agent", &key).await?;
                let mut agent = previous.clone().unwrap_or_else(|| Agent {
                    discovery: Default::default(),
                    enrollment: TeamEnrollment {
                        address,
                        alias: runtime.alias.clone(),
                        manually_held: runtime.manually_held,
                        continuation_turn_id: runtime.continuation_turn_id.clone(),
                    },
                    requested_selection: None,
                    runtime: None,
                    status: "unloaded".into(),
                    current_model: None,
                    current_effort: None,
                    next_model: None,
                    next_effort: None,
                    project: None,
                });
                agent.enrollment.alias = runtime.alias.clone();
                agent.enrollment.manually_held = runtime.manually_held;
                agent.enrollment.continuation_turn_id = runtime.continuation_turn_id.clone();
                agent.current_model = runtime.current_model.clone();
                agent.current_effort = runtime.current_effort.clone();
                agent.next_model = runtime.next_model.clone();
                agent.next_effort = runtime.next_effort.clone();
                if agent.requested_selection.as_ref().is_some_and(|request| {
                    runtime
                        .superseded_selection_revision
                        .is_some_and(|revision| revision >= request.revision)
                }) {
                    agent.requested_selection = None;
                }
                agent.status = if runtime.archived {
                    "archived"
                } else if !runtime.loaded {
                    "unloaded"
                } else if runtime.turn_id.is_some() {
                    "working"
                } else {
                    "idle"
                }
                .into();
                agent.runtime = Some(runtime);
                let _mutation = self.mutations.lock().await;
                if self
                    .observed_versions
                    .lock()
                    .await
                    .get(&server_key(&machine.id, &server.id))
                    .copied()
                    .unwrap_or_default()
                    != version
                {
                    continue;
                }
                if previous.as_ref() != Some(&agent) {
                    self.store.put("agent", &key, &agent).await?;
                    self.notify();
                }
            }
            let Some(next) = host.next_cursor else { break };
            cursor = Some(next);
        }
        let _mutation = self.mutations.lock().await;
        if self
            .observed_versions
            .lock()
            .await
            .get(&server_key(&machine.id, &server.id))
            .copied()
            .unwrap_or_default()
            == initial_version
        {
            let mut departed = std::collections::HashSet::new();
            for agent in self.agents().await? {
                let address = &agent.enrollment.address;
                let key = address_key(address);
                if address.machine_id == machine.id
                    && address.server_id == server.id
                    && !present.contains(&key)
                {
                    departed.insert(key);
                }
            }
            if !departed.is_empty() {
                self.forget_agents(&departed).await?;
                self.notify();
            }
        }
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn observe(
        &self,
        machine: &Machine,
        server: &Server,
        notification: ServerNotification,
    ) -> Result<()> {
        let thread_id = match &notification {
            ServerNotification::ThreadStatusChanged(value) => &value.thread_id,
            ServerNotification::ThreadSettingsUpdated(value) => &value.thread_id,
            ServerNotification::TurnStarted(value) => &value.thread_id,
            ServerNotification::TurnCompleted(value) => &value.thread_id,
            ServerNotification::ThreadArchived(value) => &value.thread_id,
            ServerNotification::ThreadUnarchived(value) => &value.thread_id,
            _ => return Ok(()),
        };
        let key = address_key(&TeamAddress {
            machine_id: machine.id.clone(),
            server_id: server.id.clone(),
            thread_id: thread_id.clone(),
        });
        let _mutation = self.mutations.lock().await;
        *self
            .observed_versions
            .lock()
            .await
            .entry(server_key(&machine.id, &server.id))
            .or_default() += 1;
        let Some(mut agent) = self.store.get::<Agent>("agent", &key).await? else {
            return Ok(());
        };
        let previous = agent.clone();
        match notification {
            ServerNotification::ThreadArchived(_) => {
                agent.status = "archived".into();
                if let Some(runtime) = agent.runtime.as_mut() {
                    runtime.archived = true;
                    runtime.loaded = false;
                    runtime.activity = TeamActivity::Archived;
                    runtime.turn_id = None;
                    runtime.current_model = None;
                    runtime.current_effort = None;
                    for environment in &mut runtime.environments {
                        environment.current = false;
                    }
                }
            }
            ServerNotification::ThreadUnarchived(_) => {
                agent.status = "unloaded".into();
                if let Some(runtime) = agent.runtime.as_mut() {
                    runtime.archived = false;
                    runtime.loaded = false;
                    runtime.activity = TeamActivity::Unloaded;
                }
            }
            ServerNotification::ThreadStatusChanged(value) => {
                agent.status = status_text(&value.status);
                if let Some(runtime) = agent.runtime.as_mut() {
                    runtime.activity = (&value.status).into();
                    runtime.loaded = value.status != ThreadStatus::NotLoaded;
                    if !runtime.loaded {
                        runtime.turn_id = None;
                        runtime.current_model = None;
                        runtime.current_effort = None;
                        for environment in &mut runtime.environments {
                            environment.current = false;
                        }
                    }
                }
            }
            ServerNotification::ThreadSettingsUpdated(value) => {
                agent.next_model = Some(value.thread_settings.model);
                agent.next_effort = value
                    .thread_settings
                    .effort
                    .map(|effort| effort.to_string());
            }
            ServerNotification::TurnStarted(value) => {
                agent.status = "working".into();
                agent.current_model = None;
                agent.current_effort = None;
                if let Some(runtime) = agent.runtime.as_mut() {
                    runtime.loaded = true;
                    runtime.activity = TeamActivity::Working;
                    runtime.turn_id = Some(value.turn.id);
                    runtime.current_model = None;
                    runtime.current_effort = None;
                }
                // team/hostRead reports the settings actually captured by the
                // turn; next-turn settings may have changed concurrently.
            }
            ServerNotification::TurnCompleted(_) => {
                agent.status = "idle".into();
                agent.current_model = None;
                agent.current_effort = None;
                if let Some(runtime) = agent.runtime.as_mut() {
                    runtime.activity = TeamActivity::Idle;
                    for environment in &mut runtime.environments {
                        environment.current = false;
                    }
                    runtime.turn_id = None;
                    runtime.current_model = None;
                    runtime.current_effort = None;
                }
            }
            _ => unreachable!("notification was filtered above"),
        }
        if agent != previous {
            self.store
                .put("agent", &key, &agent)
                .await
                .context("could not persist worker status")?;
            self.notify();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "worker_sync_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "worker_metadata_tests.rs"]
mod metadata_tests;

#[cfg(test)]
#[path = "worker_protocol_tests.rs"]
mod protocol_tests;
