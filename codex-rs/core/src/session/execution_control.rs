//! Cooperative checkpoints; cancellation remains owned by the running operation.

use codex_extension_api::ExecutionPermit;

use super::session::Session;

impl Session {
    pub(crate) async fn enter_execution(&self) -> ExecutionPermit {
        self.services
            .extensions
            .execution_control()
            .enter(self.thread_id)
            .await
    }

    pub(crate) fn try_enter_execution(&self) -> Option<ExecutionPermit> {
        self.services
            .extensions
            .execution_control()
            .try_enter(self.thread_id)
    }
}
