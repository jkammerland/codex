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
    pub verified: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) struct Coordinator {
    _lock: std::fs::File,
    pub manifest: Manifest,
    pub store: TeamStore,
    pub peers: RwLock<HashMap<String, Peer>>,
    pub endpoints: RwLock<HashMap<String, ResolvedEndpoint>>,
    pub observed_versions: Mutex<HashMap<String, u64>>,
    pub statuses: RwLock<HashMap<String, ServerStatus>>,
    pub mutations: Mutex<()>,
    pub(crate) enrollments: crate::enrollment::AliasReservations,
    control_updates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
        let coordinator = Arc::new(Self {
            _lock: lock,
            manifest,
            store,
            peers: RwLock::new(HashMap::new()),
            endpoints: RwLock::new(HashMap::new()),
            observed_versions: Mutex::new(HashMap::new()),
            statuses: RwLock::new(statuses),
            mutations: Mutex::new(()),
            enrollments: crate::enrollment::AliasReservations::default(),
            control_updates: Mutex::new(HashMap::new()),
            model_updates: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        });
        coordinator.backfill_board().await?;
        Ok(coordinator)
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

    pub async fn endpoint(&self, machine_id: &str, server_id: &str) -> Result<ResolvedEndpoint> {
        self.endpoints
            .read()
            .await
            .get(&server_key(machine_id, server_id))
            .cloned()
            .context("worker transport is not available")
    }

    pub async fn agents(&self) -> Result<Vec<Agent>> {
        let control = self.current_control().await?;
        let mut agents = self
            .store
            .list::<Agent>("agent", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, agent)| agent)
            .collect::<Vec<_>>();
        let statuses = self.statuses.read().await;
        for agent in &mut agents {
            let address = &agent.enrollment.address;
            let declarations = self
                .manifest
                .machines
                .iter()
                .find(|machine| machine.id == address.machine_id)
                .and_then(|machine| {
                    machine
                        .servers
                        .iter()
                        .find(|server| server.id == address.server_id)
                })
                .map(|server| &server.environment_capabilities);
            agent.discovery = crate::discovery::project_discovery(
                agent,
                statuses.get(&server_key(&address.machine_id, &address.server_id)),
                &control,
                declarations,
            );
        }
        Ok(agents)
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
            operations: self.operator_operations().await?,
            control: self.current_control().await?,
            servers,
            agents: self.agents().await?,
            board: self.board_snapshot().await?,
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
            TeamRequest::CreateMember { .. } | TeamRequest::BoardPurge { .. } => {
                // A durable operation identity is required even for older HTTP
                // clients which submit an unwrapped request.
                Box::pin(self.execute_operation(TeamOperation::new(request))).await
            }
            TeamRequest::OperationStatus { operation_id } => Ok(TeamResponse::Operation {
                operation: self.operator_operation(&operation_id).await?,
            }),
            TeamRequest::BoardPurgePreview { scope } => Ok(TeamResponse::BoardPurgePreview {
                preview: self.preview_board_purge(&scope).await?,
            }),
            TeamRequest::MessageStatus { query } => Ok(TeamResponse::MessageStatus {
                response: self.message_status(query).await?,
            }),
            TeamRequest::ExplainAction { query } => Ok(TeamResponse::ActionExplanation {
                explanation: crate::action_explanation::explain_action(
                    &self.snapshot().await?,
                    query,
                ),
            }),
            TeamRequest::Board { request } => self.execute_board(request, None).await,
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
            TeamRequest::Reply {
                message_id,
                body,
                delivery,
            } => {
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
                    message: Box::new(self.ingest(message, delivery).await?),
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
            TeamRequest::Leave { address } => {
                self.leave(address, None).await?;
                Ok(TeamResponse::Done)
            }
            TeamRequest::Enroll { address, alias } => Ok(TeamResponse::Agent {
                agent: Box::new(self.enroll(address, alias, None).await?),
            }),
            TeamRequest::Models {
                machine_id,
                server_id,
            } => Ok(TeamResponse::Models {
                data: self.models(&machine_id, &server_id).await?,
            }),
            TeamRequest::SelectModel {
                address,
                model,
                effort,
            } => Ok(TeamResponse::Agent {
                agent: Box::new(self.select_model(address, model, effort).await?),
            }),
            TeamRequest::SetModel {
                address,
                model,
                effort,
            } => Ok(TeamResponse::Agent {
                agent: Box::new(
                    self.select_model(
                        address,
                        TeamSelection::Value { value: model },
                        effort.map_or(TeamSelection::Default, |value| TeamSelection::Value {
                            value,
                        }),
                    )
                    .await?,
                ),
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

    pub(crate) async fn control_update_lock(&self, machine: &str, server: &str) -> Arc<Mutex<()>> {
        self.control_updates
            .lock()
            .await
            .entry(server_key(machine, server))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
        self.store
            .put("models", &server_key(machine_id, server_id), &data)
            .await?;
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
