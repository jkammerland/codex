use anyhow::Result;
use codex_app_server_protocol::TeamCancellationAdapter;
use codex_app_server_protocol::TeamJobStatus;
use codex_app_server_protocol::TeamTrackedJob;
use codex_extension_api::McpToolContext;
use codex_extension_api::McpToolResultInput;
use codex_extension_api::McpToolSource;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

use crate::TeamHostService;

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredJob {
    pub job: TeamTrackedJob,
    pub adapter: Option<TeamCancellationAdapter>,
    #[serde(default)]
    pub launch_revision: Option<i64>,
    pub requested_revision: Option<i64>,
    pub attempted_revision: Option<i64>,
    pub registration_fingerprint: Option<String>,
}

fn matches(adapter: &TeamCancellationAdapter, context: &McpToolContext) -> bool {
    let tool = context.tool_info();
    adapter.server == tool.server_name
        && adapter.launch_tool == tool.tool.name
        && match (adapter.plugin_id.as_deref(), context.source()) {
            (None, McpToolSource::Config) => true,
            (Some(expected), McpToolSource::Plugin { id, .. }) => expected == id,
            _ => false,
        }
}

impl TeamHostService {
    pub(crate) async fn jobs(&self, thread_id: &str) -> Result<Vec<TeamTrackedJob>> {
        let Some(store) = self.store.get() else {
            return Ok(Vec::new());
        };
        Ok(store
            .list::<StoredJob>("job", "", u32::MAX)
            .await?
            .into_iter()
            .filter(|(_, stored)| stored.job.thread_id == thread_id)
            .map(|(_, stored)| stored.job)
            .collect())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn track_launch(&self, input: ToolStartInput<'_>) -> Result<()> {
        let thread_id = ThreadId::from_string(input.thread_store.level_id())?;
        let Some(root) = self.gate.root(thread_id) else {
            return Ok(());
        };
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let Some(context) = input.mcp_tool else {
            return Ok(());
        };
        let adapters = store
            .get::<Vec<TeamCancellationAdapter>>("configuration", "cancellation_adapters")
            .await?
            .unwrap_or_default();
        let Some(adapter) = adapters
            .into_iter()
            .find(|adapter| matches(adapter, context))
        else {
            return Ok(());
        };
        let id = format!("{}:{}:{}", thread_id, input.turn_id, input.call_id);
        let _update = self.job_updates.lock().await;
        if store.get::<StoredJob>("job", &id).await?.is_some() {
            input
                .turn_store
                .get_or_init(codex_extension_api::RetainedToolResults::default)
                .retain(input.call_id.to_owned());
            return Ok(());
        }
        let launch_revision = input
            .turn_store
            .get::<codex_extension_api::ExecutionAdmissions>()
            .and_then(|admissions| admissions.revision(input.call_id))
            .map(i64::try_from)
            .transpose()?;
        let mut requested_revision = None;
        for (_, control) in store
            .list::<codex_app_server_protocol::TeamControlState>("control_history", "", u32::MAX)
            .await?
        {
            if control.mode == codex_app_server_protocol::TeamExecutionMode::Stopped
                && launch_revision.is_none_or(|revision| revision <= control.revision)
                && store
                    .get::<Vec<String>>("control_scope", &format!("{:020}", control.revision))
                    .await?
                    .is_none_or(|scope| scope.contains(&root.to_string()))
            {
                requested_revision = Some(control.revision);
            }
        }
        store
            .put(
                "job",
                &id,
                &StoredJob {
                    job: TeamTrackedJob {
                        id: id.clone(),
                        thread_id: thread_id.to_string(),
                        server: adapter.server.clone(),
                        tool: adapter.launch_tool.clone(),
                        handle: None,
                        status: TeamJobStatus::Unconfirmed,
                        detail: Some(
                            "Launch is in flight; no job handle has been returned.".into(),
                        ),
                    },
                    adapter: Some(adapter),
                    launch_revision,
                    requested_revision,
                    attempted_revision: None,
                    registration_fingerprint: context.registration_fingerprint().map(str::to_owned),
                },
            )
            .await?;
        input
            .turn_store
            .get_or_init(codex_extension_api::RetainedToolResults::default)
            .retain(input.call_id.to_owned());
        self.notify();
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    async fn track_result(&self, input: &McpToolResultInput<'_>) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let id = format!(
            "{}:{}:{}",
            input.thread_store.level_id(),
            input.turn_id,
            input.call_id
        );
        let update = self.job_updates.lock().await;
        let Some(mut stored) = store.get::<StoredJob>("job", &id).await? else {
            return Ok(());
        };
        let Some(adapter) = &stored.adapter else {
            return Ok(());
        };
        if !matches(adapter, input.mcp_tool) {
            return Ok(());
        }
        // Only the configured structured field establishes ownership. Tool
        // prose, arbitrary IDs in messages and another plugin's output do not.
        let handle = input
            .result
            .structured_content
            .as_ref()
            .and_then(|value| value.pointer(&adapter.job_id_pointer))
            .and_then(serde_json::Value::as_str);
        if let Some(handle) = handle {
            stored.job.handle = Some(handle.to_string());
            stored.job.status = TeamJobStatus::Running;
            stored.job.detail = None;
        } else {
            stored.job.detail = Some(
                "Launch returned no configured job handle; inspect the external service.".into(),
            );
        }
        store.put("job", &id, &stored).await?;
        drop(update);
        self.notify();
        self.cancel_jobs().await?;
        Ok(())
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn request_job_cancellation(
        &self,
        revision: i64,
        roots: &[String],
    ) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let update = self.job_updates.lock().await;
        for (id, mut stored) in store.list::<StoredJob>("job", "", u32::MAX).await? {
            if stored.job.status != TeamJobStatus::Cancelled
                && stored
                    .launch_revision
                    .is_none_or(|launched| launched <= revision)
                && ThreadId::from_string(&stored.job.thread_id)
                    .ok()
                    .and_then(|id| self.gate.root(id))
                    .is_some_and(|root| roots.contains(&root.to_string()))
            {
                stored.requested_revision = Some(revision);
                store.put("job", &id, &stored).await?;
            }
        }
        drop(update);
        self.cancel_jobs().await
    }

    /// Cancellation acknowledgements are independent of admission controls.
    /// A slow external service cannot block the Pause/Resume RPC path.
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn cancel_jobs(&self) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let _update = self.job_updates.lock().await;
        for (id, mut stored) in store.list::<StoredJob>("job", "", u32::MAX).await? {
            if stored.job.status == TeamJobStatus::Cancelled {
                continue;
            }
            if stored.requested_revision.is_none()
                || (stored.attempted_revision == stored.requested_revision
                    && stored.job.status != TeamJobStatus::CancellationPending)
            {
                continue;
            }
            let (Some(adapter), Some(handle), Some(fingerprint)) = (
                stored.adapter.clone(),
                stored.job.handle.clone(),
                stored.registration_fingerprint.clone(),
            ) else {
                continue;
            };
            let Ok(thread) = manager
                .get_thread(ThreadId::from_string(&stored.job.thread_id)?)
                .await
            else {
                continue;
            };
            if !self.cancellations.lock().await.insert(id.clone()) {
                continue;
            }
            stored.job.status = TeamJobStatus::CancellationPending;
            stored.attempted_revision = stored.requested_revision;
            stored.job.detail =
                Some("Cancellation requested; awaiting external confirmation.".into());
            if let Err(error) = store.put("job", &id, &stored).await {
                self.cancellations.lock().await.remove(&id);
                return Err(error);
            }
            let mut arguments = adapter.cancel_arguments.clone();
            arguments[&adapter.cancel_id_field] = handle.into();
            let store = store.clone();
            let changed = self.changed.clone();
            let cancellations = self.cancellations.clone();
            let job_updates = self.job_updates.clone();
            let events = self.events.clone();
            tokio::spawn(async move {
                let result = thread
                    .cancel_team_mcp_job(
                        &adapter.server,
                        &adapter.cancel_tool,
                        adapter.plugin_id.as_deref(),
                        &fingerprint,
                        arguments,
                    )
                    .await;
                let (status, detail) =
                    match result {
                        Ok(result)
                            if !result.is_error.unwrap_or(false)
                                && result.structured_content.as_ref().and_then(|value| {
                                    value.pointer(&adapter.confirmation_pointer)
                                }) == Some(&adapter.confirmation_value) =>
                        {
                            (TeamJobStatus::Cancelled, None)
                        }
                        Ok(_) => (
                            TeamJobStatus::Unconfirmed,
                            Some("The external service has not confirmed cancellation.".into()),
                        ),
                        Err(error) => (
                            TeamJobStatus::Unconfirmed,
                            Some(format!("Cancellation failed: {error}")),
                        ),
                    };
                let _update = job_updates.lock().await;
                // A later Stop may have arrived while this RPC was pending.
                // Merge its durable intent instead of overwriting it.
                let saved = async {
                    if let Some(mut latest) = store.get::<StoredJob>("job", &id).await? {
                        latest.job.status = status;
                        latest.job.detail = detail;
                        store.put("job", &id, &latest).await?;
                    }
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = saved {
                    tracing::error!(%error, "could not persist external cancellation status");
                }
                cancellations.lock().await.remove(&id);
                changed.send_modify(|revision| *revision += 1);
                let _ = events.send(crate::lifecycle::HostEvent::Dispatch);
            });
            self.notify();
        }
        Ok(())
    }
}

impl ToolLifecycleContributor for TeamHostService {
    fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            let admission = input.admission;
            if let Err(error) = self.track_launch(input).await {
                admission.reject(format!(
                    "could not persist external team launch intent: {error}"
                ));
                tracing::error!(%error, "could not track external team work");
            }
        })
    }

    fn on_mcp_tool_result<'a>(&'a self, input: McpToolResultInput<'a>) -> ToolLifecycleFuture<'a> {
        Box::pin(async move {
            if let Err(error) = self.track_result(&input).await {
                tracing::error!(%error, "could not persist external team job handle");
            }
        })
    }
}
