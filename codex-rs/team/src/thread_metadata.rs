//! Coordinator projections leave host-owned paths uninterpreted on foreign platforms.

use codex_app_server_protocol::ThreadStatus;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadMetadata {
    pub status: ThreadStatus,
    pub project_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ThreadReadMetadata {
    pub thread: ThreadMetadata,
}
