use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamControlState;
use codex_app_server_protocol::TeamEnrollParams;
use codex_app_server_protocol::TeamEnrollResponse;
use codex_app_server_protocol::TeamEnrollment;
use codex_app_server_protocol::TeamHostReadParams;
use codex_app_server_protocol::TeamHostReadResponse;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_extension_api::ExecutionControl;
use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_team_store::TeamStore;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::mpsc;
use tokio::sync::watch;
use uuid::Uuid;

use crate::control::execution_mode;
use crate::lifecycle::HostEvent;

/// Loaded before any thread can be recovered by the app server.
pub struct PreparedTeamHost {
    gate: ExecutionControl,
    store: Option<TeamStore>,
    path: PathBuf,
    sqlite: SqliteConfig,
    installation_id: String,
}

impl PreparedTeamHost {
    pub async fn load(config: &Config, installation_id: String) -> Result<Self> {
        let path = config.sqlite.home().join("team").join("host.sqlite");
        let gate = ExecutionControl::default();
        let store = if tokio::fs::try_exists(&path).await? {
            let store = TeamStore::open(&config.sqlite, &path).await?;
            if let Some(control) = store.control().await? {
                for enrollment in store.enrollments("", u32::MAX).await? {
                    gate.restore(
                        ThreadId::from_string(&enrollment.address.thread_id)?,
                        u64::try_from(control.revision)?,
                        execution_mode(control.mode),
                    );
                    if enrollment.manually_held {
                        gate.hold_manually(ThreadId::from_string(&enrollment.address.thread_id)?);
                    }
                }
            }
            for (thread_id, held) in store.list::<bool>("manual_hold", "", u32::MAX).await? {
                if held {
                    gate.hold_manually(ThreadId::from_string(&thread_id)?);
                }
            }
            for (_, member) in store
                .list::<codex_app_server_protocol::TeamAgentRuntime>("member", "", u32::MAX)
                .await?
            {
                if member.thread_id != member.root_thread_id {
                    gate.inherit(
                        ThreadId::from_string(&member.thread_id)?,
                        ThreadId::from_string(
                            member
                                .parent_thread_id
                                .as_deref()
                                .unwrap_or(&member.root_thread_id),
                        )?,
                    );
                }
            }
            Some(store)
        } else {
            None
        };
        Ok(Self {
            gate,
            store,
            path,
            sqlite: config.sqlite.clone(),
            installation_id,
        })
    }
}

pub struct TeamHostService {
    pub(crate) cancellations: Arc<Mutex<std::collections::HashSet<String>>>,
    pub(crate) delivery_updates: Mutex<()>,
    pub(crate) job_updates: Arc<Mutex<()>>,
    pub(crate) gate: ExecutionControl,
    pub(crate) store: OnceCell<TeamStore>,
    pub(crate) manager: Weak<ThreadManager>,
    pub(crate) operations: Mutex<()>,
    pub(crate) control_updates: Mutex<()>,
    pub(crate) model_updates: Mutex<()>,
    // Stored Running state needs a fresh coordinator confirmation after restart.
    pub(crate) confirmed_control_revision: std::sync::atomic::AtomicI64,
    pub(crate) control_events: mpsc::UnboundedSender<()>,
    pub(crate) events: mpsc::UnboundedSender<HostEvent>,
    pub(crate) changed: watch::Sender<i64>,
    path: PathBuf,
    sqlite: SqliteConfig,
    installation_id: String,
}

impl TeamHostService {
    pub fn new(prepared: PreparedTeamHost, manager: Weak<ThreadManager>) -> Arc<Self> {
        let (events, receiver) = mpsc::unbounded_channel();
        let (control_events, control_receiver) = mpsc::unbounded_channel();
        let service = Arc::new(Self {
            cancellations: Arc::new(Mutex::new(std::collections::HashSet::new())),
            delivery_updates: Mutex::new(()),
            job_updates: Arc::new(Mutex::new(())),
            gate: prepared.gate,
            store: OnceCell::new_with(prepared.store),
            manager,
            operations: Mutex::new(()),
            control_updates: Mutex::new(()),
            model_updates: Mutex::new(()),
            confirmed_control_revision: std::sync::atomic::AtomicI64::new(-1),
            control_events,
            events,
            changed: watch::channel(0).0,
            path: prepared.path,
            sqlite: prepared.sqlite,
            installation_id: prepared.installation_id,
        });
        tokio::spawn(Self::run(Arc::downgrade(&service), receiver));
        tokio::spawn(Self::run_controls(
            Arc::downgrade(&service),
            control_receiver,
        ));
        let _ = service.control_events.send(());
        let mut gate_changes = service.gate.subscribe();
        let weak = Arc::downgrade(&service);
        tokio::spawn(async move {
            while gate_changes.changed().await.is_ok() {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                service.notify();
            }
        });
        service
    }

    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.changed.subscribe()
    }

    pub(crate) fn notify(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    pub(crate) async fn enable(&self) -> Result<&TeamStore> {
        self.store
            .get_or_try_init(|| TeamStore::open(&self.sqlite, &self.path))
            .await
    }

    pub(crate) async fn require_coordinator(&self, coordinator_id: &str) -> Result<&TeamStore> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let control = store
            .control()
            .await?
            .ok_or_else(|| anyhow::anyhow!("team control is missing"))?;
        ensure!(
            control.coordinator_id == coordinator_id,
            "coordinator identity does not match enrollment"
        );
        Ok(store)
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub async fn read(&self, params: TeamHostReadParams) -> Result<TeamHostReadResponse> {
        // Runtime discovery also persists newly seen descendants and their
        // credentials. Concurrent readers must not replace those credentials.
        let _operation = self.operations.lock().await;
        let limit = params.limit.unwrap_or(100).clamp(1, 1000);
        let (control, mut data) = match self.store.get() {
            Some(store) => (
                store.control().await?,
                store
                    .enrollments(params.cursor.as_deref().unwrap_or(""), limit + 1)
                    .await?,
            ),
            None => (None, Vec::new()),
        };
        let next_cursor = if data.len() > limit as usize {
            data.truncate(limit as usize);
            data.last().map(|entry| entry.address.thread_id.clone())
        } else {
            None
        };
        let runtime = self.runtime(&data).await?;
        Ok(TeamHostReadResponse {
            installation_id: self.installation_id.clone(),
            protocol_version: 4,
            control,
            data,
            runtime,
            next_cursor,
        })
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub async fn enroll(&self, mut params: TeamEnrollParams) -> Result<TeamEnrollResponse> {
        params.address.thread_id = ThreadId::from_string(&params.address.thread_id)?.to_string();
        let operation = params
            .operation_id
            .as_ref()
            .map(|id| {
                (
                    format!("{}:{id}", params.control.coordinator_id),
                    serde_json::to_string(&params),
                )
            })
            .map(|(id, digest)| digest.map(|digest| (id, digest)))
            .transpose()?;
        let _operation = self.operations.lock().await;
        let store = self.enable().await?;
        if let Some((id, digest)) = &operation
            && let Some(enrollment) = store.host_operation_result(id, digest).await?
        {
            if let Some(control) = store.control().await? {
                self.apply_gate(&control).await?;
            }
            return Ok(TeamEnrollResponse { enrollment });
        }
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("host is shutting down"))?;
        let thread_id = ThreadId::from_string(&params.address.thread_id)?;
        params.address.thread_id = thread_id.to_string();
        let thread = manager.get_thread(thread_id).await?;
        let snapshot = thread.config_snapshot().await;
        ensure!(
            snapshot.parent_thread_id.is_none()
                && ThreadId::from(thread.startup_metadata().session_id) == thread_id,
            "only root sessions may be enrolled"
        );
        ensure!(
            !thread.config().await.ephemeral,
            "enrollment requires a durable session"
        );
        ensure!(
            !params.alias.trim().is_empty(),
            "an agent alias is required"
        );
        thread.ensure_rollout_materialized().await;
        thread.flush_rollout().await?;
        let _control = self.control_updates.lock().await;
        let store = self.enable().await?;
        let accepted_control = store.apply_control(&params.control).await?;
        if accepted_control == params.control {
            self.confirmed_control_revision.fetch_max(
                accepted_control.revision,
                std::sync::atomic::Ordering::AcqRel,
            );
        }
        let enrollment = store
            .enroll_operation(
                &TeamEnrollment {
                    address: params.address,
                    alias: params.alias,
                    manually_held: false,
                    continuation_turn_id: None,
                },
                &Uuid::new_v4().to_string(),
                operation
                    .as_ref()
                    .map(|(id, digest)| (id.as_str(), digest.as_str())),
            )
            .await?;
        if let Some(control) = store.control().await? {
            self.apply_gate(&control).await?;
        }
        let _ = self.control_events.send(());
        self.notify();
        let _ = self.events.send(HostEvent::Dispatch);
        Ok(TeamEnrollResponse { enrollment })
    }

    pub(crate) async fn apply_gate(&self, control: &TeamControlState) -> Result<()> {
        if let Some(store) = self.store.get() {
            let stopping = !self.pending_stops().await?.is_empty();
            let unconfirmed = self
                .confirmed_control_revision
                .load(std::sync::atomic::Ordering::Acquire)
                < control.revision;
            let model_errors = store
                .list::<String>("model_recovery_error", "", u32::MAX)
                .await?;
            for enrollment in store.enrollments("", u32::MAX).await? {
                let root = ThreadId::from_string(&enrollment.address.thread_id)?;
                let leaving = store
                    .get::<bool>("leaving", &enrollment.address.thread_id)
                    .await?
                    .unwrap_or(false);
                let reconciling = stopping
                    || unconfirmed
                    || leaving
                    || model_errors.iter().any(|(thread_id, _)| {
                        thread_id == &enrollment.address.thread_id
                            || ThreadId::from_string(thread_id)
                                .ok()
                                .and_then(|id| self.gate.root(id))
                                == Some(root)
                    });
                self.gate.apply_reconciling(
                    root,
                    u64::try_from(control.revision)?,
                    execution_mode(control.mode),
                    reconciling,
                );
            }
        }
        Ok(())
    }
}
