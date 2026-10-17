//! Opt-in team coordinator and operator clients. Worker history stays on its host.

mod cli;
mod client;
mod config;
mod coordinator;
mod enrollment;
mod gitea;
mod gitea_publish;
mod http;
mod mcp;
mod mcp_stdio;
mod model;
mod model_policy;
mod operations;
mod operator_mcp;
mod routing;
mod startup;
mod thread_metadata;
mod transport;
mod worker;
mod worker_outbox;
mod worker_sync;

pub use cli::TeamAction;
pub use cli::TeamCommand;
pub use client::TeamClient;
pub use config::Endpoint;
pub use config::Machine;
pub use config::Manifest;
pub use config::Server;
pub use model::*;
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
