//! Reserve session identity before startup and retain uncertainty across restart.

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamMemberCreateParams;
use codex_app_server_protocol::TeamMemberCreatePhase;
use codex_app_server_protocol::TeamMemberCreateResponse;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::TeamHostService;
use crate::control::execution_mode;

/// A host-owned reservation, retained even when startup cannot report its outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemberCreateIntent {
    pub params: TeamMemberCreateParams,
    pub thread_id: String,
    pub starting: bool,
    pub outcome: Option<TeamMemberCreateResponse>,
}

fn key(params: &TeamMemberCreateParams) -> String {
    format!("{}:{}", params.coordinator_id, params.operation_id)
}

impl TeamHostService {
    /// Serialize only retries of this creation; control and unrelated roots stay responsive.
    pub async fn lock_member_creation(
        &self,
        params: &TeamMemberCreateParams,
    ) -> Result<OwnedMutexGuard<()>> {
        ensure!(
            Uuid::parse_str(&params.operation_id)?.to_string() == params.operation_id,
            "member creation operation ID must be a canonical UUID"
        );
        ensure!(
            params.installation_id == self.installation_id,
            "member creation installation identity does not match this worker"
        );
        self.require_coordinator(&params.coordinator_id).await?;
        let lock = self
            .member_create_updates
            .lock()
            .await
            .entry(key(params))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        Ok(lock.lock_owned().await)
    }

    /// Call while holding this operation's creation lock, before any startup work.
    pub async fn prepare_member_creation(
        &self,
        params: TeamMemberCreateParams,
    ) -> Result<MemberCreateIntent> {
        let store = self.require_coordinator(&params.coordinator_id).await?;
        if let Some(intent) = store
            .get::<MemberCreateIntent>("member_create", &key(&params))
            .await?
        {
            ensure!(
                intent.params == params,
                "member creation operation identity has conflicting content"
            );
            return Ok(intent);
        }
        let manager = self.manager.upgrade().context("host is shutting down")?;
        let intent = MemberCreateIntent {
            thread_id: manager.reserve_thread_id().to_string(),
            params,
            starting: false,
            outcome: None,
        };
        store
            .put("member_create", &key(&intent.params), &intent)
            .await?;

        // A visible or recovered root may not run before coordinator enrollment.
        let control = store.control().await?.context("team control is missing")?;
        self.gate.restore(
            ThreadId::from_string(&intent.thread_id)?,
            u64::try_from(control.revision)?,
            execution_mode(control.mode),
        );
        self.notify();
        Ok(intent)
    }

    /// Persist the ambiguity boundary immediately before invoking core startup.
    pub async fn mark_member_creation_starting(
        &self,
        params: &TeamMemberCreateParams,
    ) -> Result<()> {
        let store = self.require_coordinator(&params.coordinator_id).await?;
        let mut intent = store
            .get::<MemberCreateIntent>("member_create", &key(params))
            .await?
            .context("member creation reservation is missing")?;
        ensure!(
            intent.params == *params,
            "member creation operation identity has conflicting content"
        );
        ensure!(
            !intent.starting && intent.outcome.is_none(),
            "member creation startup already began"
        );
        intent.starting = true;
        store.put("member_create", &key(params), &intent).await?;
        self.notify();
        Ok(())
    }

    /// Publish a content-free durable outcome while retaining the original reservation.
    pub async fn finish_member_creation(
        &self,
        params: &TeamMemberCreateParams,
        phase: TeamMemberCreatePhase,
        error: Option<String>,
    ) -> Result<TeamMemberCreateResponse> {
        let store = self.require_coordinator(&params.coordinator_id).await?;
        let mut intent = store
            .get::<MemberCreateIntent>("member_create", &key(params))
            .await?
            .context("member creation reservation is missing")?;
        ensure!(
            intent.params == *params,
            "member creation operation identity has conflicting content"
        );
        let response = TeamMemberCreateResponse {
            operation_id: params.operation_id.clone(),
            thread_id: intent.thread_id.clone(),
            phase,
            error,
        };
        intent.outcome = Some(response.clone());
        store.put("member_create", &key(params), &intent).await?;
        if phase == TeamMemberCreatePhase::Failed {
            self.gate
                .unenroll(ThreadId::from_string(&intent.thread_id)?);
        }
        self.notify();
        Ok(response)
    }
}
