//! Agent administration uses the same durable host outbox as collaboration.

use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::*;
use serde_json::Value;

use crate::TeamHostService;

impl TeamHostService {
    pub(crate) async fn agent_member_action(
        &self,
        sender: &TeamAddress,
        mut request: TeamMemberActionParams,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        if let TeamMemberActionParams::Status { operation_id } = &request {
            return self.administration_status(operation_id).await;
        }
        if let TeamMemberActionParams::Rename { address, .. }
        | TeamMemberActionParams::Remove { address } = &mut request
        {
            *address = self.resolve_team_target(address).await?.0.into();
        }
        self.queue_administration(
            sender,
            TeamAdministrationRequest::Member(request),
            operation,
        )
        .await
    }

    pub(crate) async fn agent_cleanup_action(
        &self,
        sender: &TeamAddress,
        request: TeamCleanupParams,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        if let TeamCleanupParams::Status { operation_id } = &request {
            return self.administration_status(operation_id).await;
        }
        self.queue_administration(
            sender,
            TeamAdministrationRequest::Cleanup(request),
            operation,
        )
        .await
    }

    async fn administration_status(&self, operation_id: &str) -> Result<Value> {
        let store = self.store.get().context("team host is not enrolled")?;
        let operation = store
            .get::<TeamAdministrationOperation>("administration_cache", operation_id)
            .await?
            .or(store.get("administration_local", operation_id).await?)
            .context("administration operation is not synchronized on this host")?;
        Ok(serde_json::to_value(operation)?)
    }

    async fn queue_administration(
        &self,
        sender: &TeamAddress,
        request: TeamAdministrationRequest,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        let store = self.store.get().context("team host is not enrolled")?;
        let mut queued = TeamAdministrationOperation {
            operation_id: uuid::Uuid::now_v7().to_string(),
            actor: sender.clone(),
            request,
            state: TeamAdministrationState::Queued,
            waiting_reason: None,
            result: None,
        };
        let payload = serde_json::to_value(&queued)?;
        let result = if let Some((id, digest)) = operation {
            store
                .enqueue_agent_operation(
                    id,
                    digest,
                    &queued.operation_id,
                    "administration",
                    &payload,
                    &payload,
                )
                .await?
        } else {
            store
                .enqueue(&queued.operation_id, "administration", &payload)
                .await?;
            payload
        };
        // Recover the original queued identity when the caller repeats a lost response.
        queued = serde_json::from_value(result.clone())?;
        store
            .put("administration_local", &queued.operation_id, &queued)
            .await?;
        self.notify();
        Ok(result)
    }

    /// Called under the host operation lock before dispatch can observe sync.
    pub(crate) async fn sync_administration(
        &self,
        operations: &[TeamAdministrationOperation],
        retirements: &[TeamMemberRetirement],
    ) -> Result<()> {
        let store = self.store.get().context("team host is not enrolled")?;
        for operation in operations {
            store
                .put("administration_cache", &operation.operation_id, operation)
                .await?;
        }
        for retirement in retirements {
            self.apply_retirement(retirement).await?;
        }
        self.advance_retirements().await
    }
}
