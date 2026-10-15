use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_team_store::TeamStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::watch;
use uuid::Uuid;

use crate::Manifest;
use crate::model::*;
use crate::transport::ResolvedEndpoint;

#[derive(Clone)]
pub(crate) struct Peer {
    pub handle: AppServerRequestHandle,
    pub endpoint: ResolvedEndpoint,
}

pub(crate) struct Coordinator {
    _lock: std::fs::File,
    pub manifest: Manifest,
    pub store: TeamStore,
    pub peers: RwLock<HashMap<String, Peer>>,
    pub statuses: RwLock<HashMap<String, ServerStatus>>,
    pub mutations: Mutex<()>,
    enrollments: Mutex<()>,
    model_updates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub changed: watch::Sender<u64>,
}

pub(crate) fn request_id() -> RequestId {
    RequestId::String(Uuid::now_v7().to_string())
}

impl Coordinator {
    pub async fn open(manifest: Manifest) -> Result<Arc<Self>> {
        let mut directory = tokio::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        directory.mode(0o700);
        directory.create(&manifest.coordinator.state_dir).await?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(manifest.coordinator.state_dir.join("coordinator.lock"))?;
        lock.try_lock()
            .context("another coordinator owns this state directory")?;
        let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(
            manifest.coordinator.state_dir.clone(),
        )?);
        let store = TeamStore::open(&sqlite, &sqlite.home().join("coordinator.sqlite")).await?;
        if store.control().await?.is_none() {
            store
                .apply_control(&TeamControlState {
                    coordinator_id: Uuid::new_v4().to_string(),
                    revision: 0,
                    mode: TeamExecutionMode::Running,
                })
                .await?;
        }
        let mut statuses = HashMap::new();
        for machine in &manifest.machines {
            for server in &machine.servers {
                statuses.insert(
                    server_key(&machine.id, &server.id),
                    ServerStatus {
                        machine_id: machine.id.clone(),
                        machine_label: machine.label.clone(),
                        server_id: server.id.clone(),
                        server_label: server.label.clone(),
                        installation_id: None,
                        connected: false,
                        applied_revision: None,
                        error: Some("Connecting".into()),
                    },
                );
            }
        }
        Ok(Arc::new(Self {
            _lock: lock,
            manifest,
            store,
            peers: RwLock::new(HashMap::new()),
            statuses: RwLock::new(statuses),
            mutations: Mutex::new(()),
            enrollments: Mutex::new(()),
            model_updates: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        }))
    }

    pub fn notify(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    pub async fn current_control(&self) -> Result<TeamControlState> {
        self.store
            .control()
            .await?
            .context("coordinator control is missing")
    }

    pub async fn peer(&self, machine_id: &str, server_id: &str) -> Result<Peer> {
        self.peers
            .read()
            .await
            .get(&server_key(machine_id, server_id))
            .cloned()
            .context("worker is not connected")
    }

    pub async fn agents(&self) -> Result<Vec<Agent>> {
        Ok(self
            .store
            .list::<Agent>("agent", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, agent)| agent)
            .collect())
    }

    pub async fn snapshot(&self) -> Result<TeamSnapshot> {
        let mut servers = self
            .statuses
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        servers.sort_by_key(|server| server_key(&server.machine_id, &server.server_id));
        Ok(TeamSnapshot {
            control: self.current_control().await?,
            servers,
            agents: self.agents().await?,
            messages: self
                .store
                .list::<MessageRecord>("message", "", u32::MAX)
                .await?
                .into_iter()
                .map(|(_, message)| message)
                .collect(),
            rejected_outbox: self
                .store
                .list::<RejectedOutbox>("rejected_outbox", "", u32::MAX)
                .await?
                .into_iter()
                .map(|(_, rejected)| rejected)
                .collect(),
        })
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub async fn execute(&self, request: TeamRequest) -> Result<TeamResponse> {
        match request {
            TeamRequest::AgentCredential { address } => {
                let peer = self.peer(&address.machine_id, &address.server_id).await?;
                let response: TeamAgentCredentialResponse = peer
                    .handle
                    .request_typed(ClientRequest::TeamAgentCredential {
                        request_id: request_id(),
                        params: TeamAgentCredentialParams {
                            thread_id: address.thread_id,
                        },
                    })
                    .await?;
                Ok(TeamResponse::AgentCredential {
                    credential: response.credential,
                })
            }
            TeamRequest::Reply { message_id, body } => {
                let original = self
                    .store
                    .get::<MessageRecord>("message", &message_id)
                    .await?
                    .context("original message is missing")?
                    .message;
                let board = original.kind == TeamMessageKind::Board;
                let message = self
                    .operator_message(
                        if board { None } else { Some(original.sender) },
                        body,
                        if board {
                            TeamMessageKind::Board
                        } else {
                            TeamMessageKind::Reply
                        },
                        Some(message_id),
                        original.topic,
                    )
                    .await?;
                Ok(TeamResponse::Message {
                    message: Box::new(self.ingest(message, TeamDeliveryMode::NextTurn).await?),
                })
            }
            TeamRequest::GiteaInspect { repository } => Ok(TeamResponse::Gitea {
                result: self.gitea_inspect(&repository).await?,
            }),
            TeamRequest::Snapshot => Ok(TeamResponse::Snapshot {
                snapshot: self.snapshot().await?,
            }),
            TeamRequest::Control { mode } => {
                let _mutation = self.mutations.lock().await;
                let mut control = self.current_control().await?;
                control.revision = control
                    .revision
                    .checked_add(1)
                    .context("control revision exhausted")?;
                control.mode = mode;
                let control = self.store.apply_control(&control).await?;
                self.notify();
                Ok(TeamResponse::Control { control })
            }
            TeamRequest::Enroll { address, alias } => Ok(TeamResponse::Agent {
                agent: Box::new(self.enroll(address, alias).await?),
            }),
            TeamRequest::Models {
                machine_id,
                server_id,
            } => Ok(TeamResponse::Models {
                data: self.models(&machine_id, &server_id).await?,
            }),
            TeamRequest::SetModel {
                address,
                model,
                effort,
            } => Ok(TeamResponse::Agent {
                agent: Box::new(self.set_model(address, model, effort).await?),
            }),
            TeamRequest::Send {
                recipient,
                body,
                kind,
                reply_to,
                delivery,
            } => {
                ensure!(
                    kind != TeamMessageKind::Board,
                    "use post for board messages"
                );
                let message = self
                    .operator_message(Some(recipient), body, kind, reply_to, None)
                    .await?;
                Ok(TeamResponse::Message {
                    message: Box::new(self.ingest(message, delivery).await?),
                })
            }
            TeamRequest::Post { topic, body } => {
                let message = self
                    .operator_message(None, body, TeamMessageKind::Board, None, Some(topic))
                    .await?;
                Ok(TeamResponse::Message {
                    message: Box::new(self.ingest(message, TeamDeliveryMode::NextTurn).await?),
                })
            }
            TeamRequest::Subscribe {
                address,
                topic,
                enabled,
            } => {
                self.subscribe(address, topic, enabled).await?;
                Ok(TeamResponse::Done)
            }
            TeamRequest::Retry { message_id } => {
                let _mutation = self.mutations.lock().await;
                let mut record = self
                    .store
                    .get::<MessageRecord>("message", &message_id)
                    .await?
                    .context("message not found")?;
                // Delivery IDs are retained. A retry cannot create another turn
                // for an already accepted host receipt.
                record.error = None;
                self.store.put("gitea_retry", &message_id, &true).await?;
                self.store.put("message", &message_id, &record).await?;
                self.notify();
                Ok(TeamResponse::Message {
                    message: Box::new(record),
                })
            }
        }
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn enroll(&self, address: TeamAddress, alias: String) -> Result<Agent> {
        // Alias reservation spans the worker RPC without holding the control
        // lock: an unreachable worker must never prevent a team-wide Pause.
        let _enrollment = self.enrollments.lock().await;
        ensure!(!alias.trim().is_empty(), "an agent alias is required");
        ensure!(!self.agents().await?.iter().any(|agent| agent.enrollment.alias == alias && agent.enrollment.address != address), "agent alias is already in use");
        let peer = self.peer(&address.machine_id, &address.server_id).await?;
        let mut cursor = None;
        let loaded = loop {
            let page: ThreadLoadedListResponse = peer
                .handle
                .request_typed(ClientRequest::ThreadLoadedList {
                    request_id: request_id(),
                    params: ThreadLoadedListParams {
                        cursor,
                        limit: Some(1000),
                    },
                })
                .await?;
            if page.data.contains(&address.thread_id) {
                break true;
            }
            let Some(next) = page.next_cursor else {
                break false;
            };
            cursor = Some(next);
        };
        if !loaded {
            let _: ThreadResumeResponse = peer
                .handle
                .request_typed(ClientRequest::ThreadResume {
                    request_id: request_id(),
                    params: ThreadResumeParams {
                        thread_id: address.thread_id.clone(),
                        ..Default::default()
                    },
                })
                .await?;
        }
        let enrolled: TeamEnrollResponse = peer
            .handle
            .request_typed(ClientRequest::TeamEnroll {
                request_id: request_id(),
                params: TeamEnrollParams {
                    address: address.clone(),
                    alias,
                    control: self.current_control().await?,
                },
            })
            .await?;
        let key = address_key(&address);
        let read: ThreadReadResponse = peer
            .handle
            .request_typed(ClientRequest::ThreadRead {
                request_id: request_id(),
                params: ThreadReadParams {
                    thread_id: address.thread_id.clone(),
                    include_turns: false,
                },
            })
            .await?;
        let agent = Agent {
            enrollment: enrolled.enrollment,
            runtime: None,
            status: status_text(&read.thread.status),
            current_model: None,
            current_effort: None,
            next_model: None,
            next_effort: None,
            project: read.thread.project_id,
        };
        self.store.put("agent", &key, &agent).await?;
        self.notify();
        Ok(agent)
    }

    pub async fn models(&self, machine_id: &str, server_id: &str) -> Result<Vec<Model>> {
        let peer = self.peer(machine_id, server_id).await?;
        let mut cursor = None;
        let mut data = Vec::new();
        loop {
            let response: ModelListResponse = peer
                .handle
                .request_typed(ClientRequest::ModelList {
                    request_id: request_id(),
                    params: ModelListParams {
                        cursor,
                        limit: Some(100),
                        include_hidden: Some(true),
                    },
                })
                .await?;
            data.extend(response.data);
            let Some(next) = response.next_cursor else {
                break;
            };
            cursor = Some(next);
        }
        Ok(data)
    }

    pub(crate) async fn model_update_lock(&self, machine: &str, server: &str) -> Arc<Mutex<()>> {
        self.model_updates
            .lock()
            .await
            .entry(server_key(machine, server))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn set_model(
        &self,
        address: TeamAddress,
        model: String,
        effort: Option<String>,
    ) -> Result<Agent> {
        let selection = self
            .model_update_lock(&address.machine_id, &address.server_id)
            .await;
        let _selection = selection.lock().await;
        let key = address_key(&address);
        self.store
            .get::<Agent>("agent", &key)
            .await?
            .context("agent is not enrolled")?;
        let models = self.models(&address.machine_id, &address.server_id).await?;
        let selected = models
            .into_iter()
            .find(|candidate| candidate.model == model)
            .context("model is unavailable on this host; select a supported model explicitly")?;
        ensure!(
            effort.is_some() || selected.default_reasoning_effort.to_string() != "ultra",
            "Ultra requires an explicit effort selection"
        );
        let effort = match effort {
            Some(effort) => selected
                .supported_reasoning_efforts
                .iter()
                .find(|option| option.reasoning_effort.to_string() == effort)
                .map(|option| option.reasoning_effort.clone())
                .context("effort is not supported by this model")?,
            None => selected.default_reasoning_effort,
        };
        let peer = self.peer(&address.machine_id, &address.server_id).await?;
        let _: ThreadSettingsUpdateResponse = peer
            .handle
            .request_typed(ClientRequest::ThreadSettingsUpdate {
                request_id: request_id(),
                params: ThreadSettingsUpdateParams {
                    thread_id: address.thread_id,
                    model: Some(model.clone()),
                    effort: Some(effort.clone()),
                    ..Default::default()
                },
            })
            .await?;
        self.store.put("model_override", &key, &true).await?;
        let _mutation = self.mutations.lock().await;
        let mut agent = self
            .store
            .get::<Agent>("agent", &key)
            .await?
            .context("agent is no longer enrolled")?;
        agent.next_model = Some(model);
        agent.next_effort = Some(effort.to_string());
        self.store.put("agent", &key, &agent).await?;
        self.notify();
        Ok(agent)
    }
}

pub(crate) fn status_text(status: &ThreadStatus) -> String {
    match status {
        ThreadStatus::NotLoaded => "unloaded",
        ThreadStatus::Idle => "idle",
        ThreadStatus::SystemError => "error",
        ThreadStatus::Active { .. } => "working",
    }
    .to_string()
}
