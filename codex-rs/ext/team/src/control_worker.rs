//! Local interruption settles behind a durable admission fence, outside RPC work.

use std::sync::Weak;

use anyhow::Result;
use codex_app_server_protocol::TeamControlState;
use codex_app_server_protocol::TeamExecutionMode;
use tokio::sync::mpsc;

use crate::TeamHostService;
use crate::lifecycle::HostEvent;

impl TeamHostService {
    pub(crate) async fn pending_stops(&self) -> Result<Vec<TeamControlState>> {
        let Some(store) = self.store.get() else {
            return Ok(Vec::new());
        };
        let mut pending = Vec::new();
        for (id, control) in store
            .list::<TeamControlState>("control_history", "", u32::MAX)
            .await?
        {
            if control.mode == TeamExecutionMode::Stopped
                && !store
                    .get::<bool>("control_cleanup_done", &id)
                    .await?
                    .unwrap_or(false)
            {
                pending.push(control);
            }
        }
        Ok(pending)
    }

    #[allow(clippy::await_holding_invalid_type)]
    pub(crate) async fn run_controls(service: Weak<Self>, mut events: mpsc::UnboundedReceiver<()>) {
        while events.recv().await.is_some() {
            let Some(service) = service.upgrade() else {
                break;
            };
            let result = async {
                for stop in service.pending_stops().await? {
                    let Some(store) = service.store.get() else {
                        continue;
                    };
                    let revision = format!("{:020}", stop.revision);
                    let scope = match store.get::<Vec<String>>("control_scope", &revision).await? {
                        Some(scope) => scope,
                        None => store
                            .enrollments("", u32::MAX)
                            .await?
                            .into_iter()
                            .map(|enrollment| enrollment.address.thread_id)
                            .collect(),
                    };
                    // Admission remains closed until old local tasks settle and
                    // cancellation intents for pre-Stop launches are durable.
                    service.stop_family(stop.revision, &scope).await?;
                    service
                        .request_job_cancellation(stop.revision, &scope)
                        .await?;
                    let _update = service.control_updates.lock().await;
                    let Some(store) = service.store.get() else {
                        continue;
                    };
                    if store
                        .get::<Vec<String>>("control_scope", &revision)
                        .await?
                        .is_none_or(|latest| latest == scope)
                    {
                        store.put("control_cleanup_done", &revision, &true).await?;
                    }
                    if let Some(control) = store.control().await? {
                        service.apply_gate(&control).await?;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                if let Some(store) = service.store.get()
                    && let Err(saved) = store
                        .put("control_cleanup_error", "latest", &error.to_string())
                        .await
                {
                    tracing::error!(%saved, "could not persist team Stop reconciliation error");
                }
                tracing::warn!(%error, "team Stop remains fenced for reconciliation");
            }
            service.notify();
            let _ = service.events.send(HostEvent::Dispatch);
        }
    }
}
