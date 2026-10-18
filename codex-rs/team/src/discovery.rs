//! Passive projections over coordinator observations. No operation in this module loads a thread.
use anyhow::Context;
use anyhow::Result;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::model::Agent;
use crate::model::ServerStatus;
use crate::model::address_key;

pub(crate) fn project_discovery(
    agent: &Agent,
    server: Option<&ServerStatus>,
    control: &TeamControlState,
    declarations: Option<&std::collections::BTreeMap<String, Vec<String>>>,
) -> TeamAgentDiscovery {
    let runtime = agent.runtime.as_ref();
    let synchronization = match server {
        Some(server) if !server.connected => TeamSynchronization::Disconnected,
        Some(server) if server.applied_revision == Some(control.revision) => {
            TeamSynchronization::Current
        }
        _ => TeamSynchronization::Unconfirmed,
    };
    let activity = if agent.status == "archived" || runtime.is_some_and(|runtime| runtime.archived)
    {
        TeamActivity::Archived
    } else {
        runtime.map_or(TeamActivity::Unknown, |runtime| {
            if !runtime.loaded {
                TeamActivity::Unloaded
            } else {
                runtime.activity
            }
        })
    };
    let mut holds = Vec::new();
    let mut reasons = Vec::new();
    match control.mode {
        TeamExecutionMode::Running => {}
        TeamExecutionMode::Paused => {
            holds.push(TeamExecutionHold::Paused);
            reasons.push("Team is paused".into());
        }
        TeamExecutionMode::Stopped => {
            holds.push(TeamExecutionHold::Stopped);
            reasons.push("Team is stopped".into());
        }
    }
    if agent.enrollment.manually_held || runtime.is_some_and(|runtime| runtime.manually_held) {
        holds.push(TeamExecutionHold::Manual);
        reasons.push("Manual or inherited hold".into());
    }
    if runtime.is_some_and(|runtime| runtime.reconciling) {
        holds.push(TeamExecutionHold::Reconciliation);
        reasons.push("Execution reconciliation is pending".into());
    }
    if agent.enrollment.continuation_turn_id.is_some()
        || runtime.is_some_and(|runtime| runtime.continuation_turn_id.is_some())
    {
        holds.push(TeamExecutionHold::Continuation);
        reasons.push("An interrupted turn has priority over new work".into());
    }
    if let Some(error) = runtime.and_then(|runtime| runtime.model_error.as_ref()) {
        holds.push(TeamExecutionHold::ModelError);
        reasons.push(error.clone());
    }
    let readiness = if synchronization != TeamSynchronization::Current {
        reasons.push(
            "Worker synchronization is unavailable or unconfirmed; host reachability is unknown"
                .into(),
        );
        TeamReadiness::Unknown
    } else if !holds.is_empty() {
        TeamReadiness::Held
    } else {
        match activity {
            TeamActivity::Idle => TeamReadiness::Ready,
            TeamActivity::Unloaded => TeamReadiness::Resumable,
            TeamActivity::Working
            | TeamActivity::WaitingForApproval
            | TeamActivity::WaitingForInput => TeamReadiness::Busy,
            TeamActivity::Unknown | TeamActivity::Error | TeamActivity::Archived => {
                TeamReadiness::Unknown
            }
        }
    };
    let mut environments = runtime
        .map(|runtime| runtime.environments.clone())
        .unwrap_or_default();
    for environment in &mut environments {
        environment.declared_capabilities = declarations
            .and_then(|declarations| declarations.get(&environment.id))
            .cloned()
            .unwrap_or_default();
    }
    TeamAgentDiscovery {
        availability: TeamAvailability {
            synchronization,
            activity,
            readiness,
            holds,
            reasons,
        },
        environments,
    }
}

impl Coordinator {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes durable snapshot revisions with coordinator mutations"
    )]
    pub(crate) async fn directory_snapshot(&self) -> Result<TeamDirectorySnapshot> {
        let _mutation = self.mutations.lock().await;
        let mut agents = self.agents().await?;
        agents.sort_by_key(|agent| address_key(&agent.enrollment.address));
        let data = agents
            .into_iter()
            .map(|agent| TeamDirectoryEntry {
                address: agent.enrollment.address,
                alias: agent.enrollment.alias,
                manually_held: agent.enrollment.manually_held,
                continuation_turn_id: agent.enrollment.continuation_turn_id,
                discovery: agent.discovery,
                current_model: agent.current_model,
                current_effort: agent.current_effort,
                next_model: agent.next_model,
                next_effort: agent.next_effort,
            })
            .collect();
        let previous = self
            .store
            .get::<TeamDirectorySnapshot>("directory_snapshot", "current")
            .await?;
        if let Some(previous) = &previous
            && previous.data == data
        {
            return Ok(previous.clone());
        }
        let revision = previous
            .map_or(Some(1), |previous| previous.revision.checked_add(1))
            .context("directory revision exhausted")?;
        let snapshot = TeamDirectorySnapshot { revision, data };
        self.store
            .put("directory_snapshot", "current", &snapshot)
            .await?;
        Ok(snapshot)
    }
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
