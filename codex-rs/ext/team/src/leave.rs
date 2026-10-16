use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamExecutionMode;
use codex_app_server_protocol::TeamLeaveParams;
use codex_app_server_protocol::TeamLeaveResponse;
use codex_protocol::ThreadId;

use crate::TeamHostService;

impl TeamHostService {
    #[allow(clippy::await_holding_invalid_type)]
    pub async fn leave(&self, mut params: TeamLeaveParams) -> Result<TeamLeaveResponse> {
        let root = ThreadId::from_string(&params.thread_id)?;
        params.thread_id = root.to_string();
        let operation = params
            .operation_id
            .as_ref()
            .map(|id| {
                (
                    format!("{}:{id}", params.coordinator_id),
                    serde_json::to_string(&params),
                )
            })
            .map(|(id, digest)| digest.map(|digest| (id, digest)))
            .transpose()?;
        let _operation = self.operations.lock().await;
        let _model = self.model_updates.lock().await;
        ensure!(
            self.gate
                .root(root)
                .is_none_or(|controlling| controlling == root),
            "only root sessions may leave a team"
        );
        let store = self.require_coordinator(&params.coordinator_id).await?;
        if let Some((id, digest)) = &operation
            && let Some(response) = store.host_operation_result(id, digest).await?
        {
            if store.enrollment(&root.to_string()).await?.is_none() {
                self.gate.unenroll(root);
            }
            return Ok(response);
        }
        if store.enrollment(&root.to_string()).await?.is_none() {
            store
                .unenroll(
                    &root.to_string(),
                    &[root.to_string()],
                    operation
                        .as_ref()
                        .map(|(id, digest)| (id.as_str(), digest.as_str())),
                )
                .await?;
            self.gate.unenroll(root);
            return Ok(TeamLeaveResponse {
                thread_id: root.to_string(),
            });
        }
        ensure!(
            self.gate
                .snapshot(root)
                .is_none_or(|state| state.in_flight == 0),
            "the enrolled family still has admitted work"
        );
        let mut members = vec![root.to_string()];
        let mut loaded = Vec::new();
        if let Some(manager) = self.manager.upgrade() {
            for thread_id in manager.list_thread_ids().await {
                if self.gate.root(thread_id) != Some(root) {
                    continue;
                }
                let thread = manager.get_thread(thread_id).await?;
                ensure!(
                    thread.team_active_turn_id().await.is_none(),
                    "the enrolled family still has a live turn"
                );
                members.push(thread_id.to_string());
                loaded.push((thread_id.to_string(), thread));
            }
        }
        for (id, member) in store
            .list::<codex_app_server_protocol::TeamAgentRuntime>("member", "", u32::MAX)
            .await?
        {
            if member.root_thread_id == root.to_string() {
                members.push(id);
            }
        }
        members.sort();
        members.dedup();
        let root_id = root.to_string();
        let mut fences = Vec::new();
        for id in &members {
            let requested = store
                .get::<serde_json::Value>("model_selection_request", id)
                .await?
                .and_then(|request| {
                    request
                        .pointer("/selection/revision")
                        .and_then(serde_json::Value::as_i64)
                });
            if let Some(revision) =
                requested.max(store.get::<i64>("model_selection_superseded", id).await?)
            {
                fences.push((
                    "model_selection_superseded",
                    id.as_str(),
                    serde_json::to_value(revision)?,
                ));
            }
        }
        {
            let _control = self.control_updates.lock().await;
            if !store
                .get::<bool>("leaving", &root.to_string())
                .await?
                .unwrap_or(false)
            {
                ensure!(
                    store
                        .control()
                        .await?
                        .is_some_and(|control| control.mode == TeamExecutionMode::Stopped),
                    "leaving requires Stop first"
                );
                ensure!(
                    self.pending_stops().await?.is_empty(),
                    "local Stop cleanup is still pending"
                );
            }
            // Keep admission closed while idempotent Core checkpoints settle.
            // A concurrent Resume is acknowledged without waiting for this work.
            fences.push(("leaving", root_id.as_str(), serde_json::json!(true)));
            store.put_records(&fences).await?;
            if let Some(control) = store.control().await? {
                self.apply_gate(&control).await?;
            }
        }
        for (id, thread) in loaded {
            let mut policy = thread.config_snapshot().await.team_model_policy;
            let fence = policy
                .selection
                .as_ref()
                .map(|selection| selection.revision)
                .max(policy.superseded_revision)
                .max(store.get::<i64>("model_selection_superseded", &id).await?);
            if let Some(revision) = fence {
                store
                    .put("model_selection_superseded", &id, &revision)
                    .await?;
                policy.superseded_revision = Some(revision);
                thread
                    .restore_thread_settings(codex_core::CodexThreadSettingsOverrides {
                        restore_model_policy: Some(policy),
                        ..Default::default()
                    })
                    .await?;
                thread.checkpoint_thread_settings().await?;
            }
        }
        store
            .unenroll(
                &root.to_string(),
                &members,
                operation
                    .as_ref()
                    .map(|(id, digest)| (id.as_str(), digest.as_str())),
            )
            .await?;
        self.gate.unenroll(root);
        self.notify();
        Ok(TeamLeaveResponse {
            thread_id: root.to_string(),
        })
    }
}
