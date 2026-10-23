//! Opt-in team coordinator and operator clients. Worker history stays on its host.

mod action_explanation;
mod administration;
mod board;
mod board_backfill;
mod board_cli;
mod board_maintenance;
mod board_subscriptions;
mod cli;
mod client;
mod config;
mod coordinator;
mod discovery;
mod enrollment;
#[cfg(test)]
#[path = "enrollment_recovery_tests.rs"]
mod enrollment_recovery_tests;
mod gitea;
mod gitea_publish;
mod http;
mod maintenance_cli;
mod mcp;
mod mcp_stdio;
mod member_cli;
mod member_creation;
mod message_status;
mod model;
mod model_policy;
mod operation_error;
mod operations;
#[cfg(test)]
mod operator_management_tests;
mod operator_mcp;
mod operator_operation;
#[cfg(test)]
mod purge_order_tests;
#[cfg(test)]
#[path = "purge_outbox_tests.rs"]
mod purge_outbox_tests;
mod retirement;
mod routing;
mod startup;
mod thread_metadata;
mod transport;
mod worker;
mod worker_outbox;
mod worker_sync;

pub use action_explanation::explain_action;
pub use board_cli::BoardAction;
pub use board_cli::BoardCommand;
pub use cli::TeamAction;
pub use cli::TeamCommand;
pub use client::TeamClient;
pub use codex_app_server_protocol::TeamBoardPurgeOperation;
pub use codex_app_server_protocol::TeamBoardPurgePreview;
pub use codex_app_server_protocol::TeamBoardPurgeScope;
pub use codex_team_store::topic_overviews;
pub use config::Endpoint;
pub use config::Machine;
pub use config::Manifest;
pub use config::Server;
pub use maintenance_cli::BoardPurgeArgs;
pub use member_cli::MemberAction;
pub use member_cli::MemberCommand;
pub use message_status::message_statuses;
pub use model::*;
pub use operation_error::TeamOperationRejected;
pub use operator_operation::*;
pub use startup::ensure_coordinator;

pub async fn run(command: TeamCommand) -> anyhow::Result<()> {
    cli::run(command).await
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;

#[cfg(test)]
#[path = "board_tests.rs"]
mod board_tests;

#[cfg(test)]
#[path = "operation_error_tests.rs"]
mod operation_error_tests;

#[cfg(test)]
#[path = "administration_tests.rs"]
mod administration_tests;
