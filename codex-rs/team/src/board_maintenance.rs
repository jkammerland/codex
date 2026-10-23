//! Online removal shares one transaction with offline maintenance.

use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::TeamBoardPurgeOperation;
use codex_app_server_protocol::TeamBoardPurgeScope;
use codex_app_server_protocol::TeamBoardPurgeWorker;
use codex_team_store::BoardPurgeCommit;
use codex_team_store::TeamStore;

use crate::coordinator::Coordinator;

pub(crate) async fn purge_workers(store: &TeamStore) -> Result<Vec<TeamBoardPurgeWorker>> {
    // Include previously synchronized installations even if their endpoint is
    // now absent. Removing a manifest entry must not imply content was erased.
    store
        .list::<String>("installation", "", u32::MAX)
        .await?
        .into_iter()
        .map(|(key, installation_id)| {
            let (machine_id, server_id) = key
                .split_once('/')
                .context("invalid stored installation address")?;
            Ok(TeamBoardPurgeWorker {
                machine_id: machine_id.into(),
                server_id: server_id.into(),
                installation_id,
                acknowledged: false,
            })
        })
        .collect()
}

impl Coordinator {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "preview counts and revision must describe one committed Board"
    )]
    pub(crate) async fn preview_board_purge(
        &self,
        scope: &TeamBoardPurgeScope,
    ) -> Result<codex_app_server_protocol::TeamBoardPurgePreview> {
        let _mutation = self.mutations.lock().await;
        self.store.preview_board_purge(scope).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "purge, retries, and message intake share the coordinator mutation order"
    )]
    pub(crate) async fn purge_board(
        &self,
        scope: TeamBoardPurgeScope,
        expected_revision: i64,
        operation_id: &str,
        digest: &str,
    ) -> Result<TeamBoardPurgeOperation> {
        let _mutation = self.mutations.lock().await;
        let result = self
            .store
            .purge_board(BoardPurgeCommit {
                scope,
                expected_revision,
                operation_id: operation_id.into(),
                digest: digest.into(),
                workers: purge_workers(&self.store).await?,
            })
            .await;
        let operation = match result {
            Ok(operation) => operation,
            Err(error) => {
                if let Some(rejection) = self
                    .store
                    .get::<crate::TeamOperationRejected>("operator_rejection", operation_id)
                    .await?
                {
                    return Err(rejection.into());
                }
                return Err(error);
            }
        };
        self.notify();
        Ok(operation)
    }
}
