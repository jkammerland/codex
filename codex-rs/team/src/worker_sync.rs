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
            host.protocol_version == 1,
            "unsupported worker team protocol"
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
        if let Some(status) = self.statuses.write().await.get_mut(&key) {
            status.installation_id = Some(host.installation_id);
        }
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

    async fn controls(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
        applied: Option<TeamControlState>,
        reconcile: bool,
    ) -> Result<()> {
        let current = self.current_control().await?;
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
            let response: TeamControlResponse = peer
                .handle
                .request_typed(ClientRequest::TeamControl {
                    request_id: request_id(),
                    params: TeamControlParams { control },
                })
                .await?;
            ensure!(
                response.reconciled,
                "worker has not acknowledged the requested control"
            );
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
            let board = self
                .store
                .list::<MessageRecord>("message", "", u32::MAX)
                .await?
                .into_iter()
                .filter(|(_, record)| {
                    record.published && record.message.kind == TeamMessageKind::Board
                })
                .map(|(_, record)| record.message)
                .collect();
            let _: TeamSyncResponse = peer
                .handle
                .request_typed(ClientRequest::TeamSync {
                    request_id: request_id(),
                    params: TeamSyncParams {
                        coordinator_id: self.current_control().await?.coordinator_id,
                        directory: self
                            .agents()
                            .await?
                            .into_iter()
                            .map(|agent| agent.enrollment)
                            .collect(),
                        board,
                        cancellation_adapters: server.cancellation_adapters.clone(),
                    },
                })
                .await?;
            if self.current_control().await?.mode == TeamExecutionMode::Running {
                self.deliver_to(&machine.id, &server.id, peer).await?;
            }
        }
        Ok(())
    }

    async fn refresh_enrollments(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
        rejoin: bool,
    ) -> Result<()> {
        let mut cursor = None;
        loop {
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
            for enrollment in host.data {
                ensure!(
                    enrollment.address.machine_id == machine.id
                        && enrollment.address.server_id == server.id,
                    "manifest identity differs from worker enrollment"
                );
                let key = address_key(&enrollment.address);
                let previous = self.store.get::<Agent>("agent", &key).await?;
                let mut agent = previous.clone().unwrap_or_else(|| Agent {
                    enrollment: enrollment.clone(),
                    runtime: None,
                    status: "unloaded".into(),
                    current_model: None,
                    current_effort: None,
                    next_model: None,
                    next_effort: None,
                    project: None,
                });
                agent.enrollment = enrollment;
                let archived = host.runtime.iter().any(|member| {
                    member.thread_id == agent.enrollment.address.thread_id && member.archived
                });
                if (rejoin || previous.is_none()) && !archived {
                    let resumed: ThreadResumeResponse = peer
                        .handle
                        .request_typed(ClientRequest::ThreadResume {
                            request_id: request_id(),
                            params: ThreadResumeParams {
                                thread_id: agent.enrollment.address.thread_id.clone(),
                                ..Default::default()
                            },
                        })
                        .await?;
                    agent.status = status_text(&resumed.thread.status);
                    agent.next_model = Some(resumed.model);
                    agent.next_effort = resumed.reasoning_effort.map(|effort| effort.to_string());
                    agent.project = resumed.thread.project_id;
                    // A reconnect does not know which settings an active turn
                    // captured. Never present its next model as the current one.
                    agent.current_model = None;
                    agent.current_effort = None;
                }
                if previous.as_ref() != Some(&agent) {
                    self.store.put("agent", &key, &agent).await?;
                    self.notify();
                }
            }
            for runtime in host.runtime {
                let address = TeamAddress {
                    machine_id: machine.id.clone(),
                    server_id: server.id.clone(),
                    thread_id: runtime.thread_id.clone(),
                };
                let key = address_key(&address);
                let previous = self.store.get::<Agent>("agent", &key).await?;
                let mut agent = previous.clone().unwrap_or_else(|| Agent {
                    enrollment: TeamEnrollment {
                        address,
                        alias: runtime.alias.clone(),
                        manually_held: runtime.manually_held,
                        continuation_turn_id: runtime.continuation_turn_id.clone(),
                    },
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
                if previous.as_ref() != Some(&agent) {
                    self.store.put("agent", &key, &agent).await?;
                    self.notify();
                }
            }
            let Some(next) = host.next_cursor else { break };
            cursor = Some(next);
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
        let Some(mut agent) = self.store.get::<Agent>("agent", &key).await? else {
            return Ok(());
        };
        let previous = agent.clone();
        match notification {
            ServerNotification::ThreadArchived(_) => agent.status = "archived".into(),
            ServerNotification::ThreadUnarchived(_) => agent.status = "unloaded".into(),
            ServerNotification::ThreadStatusChanged(value) => {
                agent.status = status_text(&value.status)
            }
            ServerNotification::ThreadSettingsUpdated(value) => {
                agent.next_model = Some(value.thread_settings.model);
                agent.next_effort = value
                    .thread_settings
                    .effort
                    .map(|effort| effort.to_string());
            }
            ServerNotification::TurnStarted(_) => {
                agent.status = "working".into();
                // team/hostRead reports the settings actually captured by the
                // turn; next-turn settings may have changed concurrently.
            }
            ServerNotification::TurnCompleted(_) => {
                agent.status = "idle".into();
                agent.current_model = None;
                agent.current_effort = None;
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
