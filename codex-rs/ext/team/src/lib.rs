//! Host-owned team enrollment, execution controls and durable message delivery.

use std::sync::Arc;

use codex_extension_api::ExtensionRegistryBuilder;

mod action_explanation;
mod board;
mod board_context;
mod bridge;
mod control;
mod control_worker;
mod delivery;
mod delivery_evidence;
mod jobs;
mod leave;
mod lifecycle;
mod message_status;
mod models;
mod runtime;
mod service;
mod tools;

pub use service::PreparedTeamHost;
pub use service::TeamHostService;

pub fn install<C: Send + Sync + 'static>(
    registry: &mut ExtensionRegistryBuilder<C>,
    service: Arc<TeamHostService>,
) {
    registry.execution_control(service.gate.clone());
    registry.tool_contributor(Arc::new(tools::TeamTools(Arc::downgrade(&service))));
    registry.turn_lifecycle_contributor(service.clone());
    registry.tool_lifecycle_contributor(service.clone());
    registry.thread_lifecycle_contributor(service);
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "model_tests.rs"]
mod model_tests;

#[cfg(test)]
#[path = "board_tests.rs"]
mod board_tests;
