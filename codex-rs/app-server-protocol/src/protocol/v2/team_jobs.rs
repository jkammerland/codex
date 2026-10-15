use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// User-owned mapping from a trusted MCP launch result to an idempotent cancel.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct TeamCancellationAdapter {
    pub server: String,
    /// Require this plugin ID, or a user-configured MCP server when absent.
    pub plugin_id: Option<String>,
    pub launch_tool: String,
    pub job_id_pointer: String,
    pub cancel_tool: String,
    pub cancel_id_field: String,
    pub cancel_arguments: Value,
    pub confirmation_pointer: String,
    pub confirmation_value: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TeamJobStatus {
    Running,
    CancellationPending,
    Cancelled,
    Unconfirmed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TeamTrackedJob {
    pub id: String,
    pub thread_id: String,
    pub server: String,
    pub tool: String,
    pub handle: Option<String>,
    pub status: TeamJobStatus,
    pub detail: Option<String>,
}
