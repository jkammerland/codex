//! Explanations report existing admission facts. They never grant authority.

use crate::TeamSnapshot;
use codex_app_server_protocol::*;

/// Explain an operator action from the same snapshot displayed by `/agents`.
pub fn explain_action(
    snapshot: &TeamSnapshot,
    query: TeamExplainActionParams,
) -> TeamActionExplanation {
    let mut explanation = TeamActionExplanation {
        action: query.action,
        target: query.target.clone(),
        availability: TeamActionAvailability::Available,
        reason:
            "Available through the existing team action; conditions are rechecked on execution."
                .into(),
        owner: "Team coordinator".into(),
        resolution: None,
        freshness: TeamObservationFreshness::Current,
    };
    let agent = query.target.as_ref().and_then(|address| {
        snapshot
            .agents
            .iter()
            .find(|agent| &agent.enrollment.address == address)
    });
    let connected = query.target.as_ref().is_some_and(|address| {
        snapshot.servers.iter().any(|server| {
            server.machine_id == address.machine_id
                && server.server_id == address.server_id
                && server.connected
                && server.applied_revision == Some(snapshot.control.revision)
        })
    });
    match query.action {
        TeamExplainableAction::Enroll => {
            explanation.owner = "Selected worker host".into();
            if agent.is_some() && connected {
                explanation.reason = "This task is already enrolled; the operator can edit its team alias.".into();
            } else {
                explanation.availability = TeamActionAvailability::Unconfirmed;
                explanation.reason = "The owning host must confirm that the selected task is an existing eligible root session before enrollment.".into();
                explanation.freshness = TeamObservationFreshness::Unconfirmed;
            }
        }
        TeamExplainableAction::Subscribe if query.target.is_some() && agent.is_none() => {
            explanation.availability = TeamActionAvailability::Blocked;
            explanation.reason = "The selected subscription identity must be an enrolled teammate.".into();
        }
        TeamExplainableAction::Post | TeamExplainableAction::Subscribe => {
            explanation.reason = "Changes can be queued while execution is held; subscriptions do not grant execution or tool permissions.".into();
        }
        TeamExplainableAction::QueueMessage => {
            if agent.is_none() {
                explanation.availability = TeamActionAvailability::Blocked;
                explanation.reason = "The recipient must be an enrolled teammate.".into();
                explanation.resolution = Some("Enroll the selected task through its owning host.".into());
            } else {
                explanation.reason = "Messages can be queued for busy, offline, or held teammates. Queueing does not admit a turn.".into();
            }
        }
        TeamExplainableAction::ProcessMessages | TeamExplainableAction::Leave | TeamExplainableAction::SelectModel => {
            explanation.owner = "Selected worker host".into();
            if let Some(agent) = agent {
                if !connected {
                    explanation.availability = TeamActionAvailability::Unconfirmed;
                    explanation.freshness = TeamObservationFreshness::Unconfirmed;
                    explanation.reason = "The worker has not confirmed the current team control revision.".into();
                    explanation.resolution = Some("Reconnect the owning host and wait for synchronization.".into());
                } else if query.action == TeamExplainableAction::SelectModel {
                    explanation.reason = "The operator can select next-turn settings from this worker's model catalog. The running turn retains its settings.".into();
                } else if query.action == TeamExplainableAction::Leave {
                    let admitted = snapshot.agents.iter().filter(|member| {
                        member.enrollment.address.machine_id == agent.enrollment.address.machine_id
                            && member.enrollment.address.server_id == agent.enrollment.address.server_id
                    }).any(|member| member.runtime.as_ref().is_some_and(|runtime| {
                        runtime.root_thread_id == agent.enrollment.address.thread_id
                            && (runtime.in_flight > 0 || runtime.turn_id.is_some())
                    }));
                    if snapshot.control.mode != TeamExecutionMode::Stopped || admitted {
                        explanation.availability = TeamActionAvailability::Blocked;
                        explanation.reason = "Leaving requires Stop first, an idle enrolled family, and settled local cleanup.".into();
                        explanation.resolution = Some("Finish or stop active work and reconcile outstanding execution first.".into());
                    } else {
                        explanation.availability = TeamActionAvailability::Unconfirmed;
                        explanation.reason = "The observed root is stopped and idle. The owning host must still confirm the entire family is idle and local Stop cleanup has settled.".into();
                    }
                } else {
                    let availability = &agent.discovery.availability;
                    explanation.reason = availability.reasons.join("; ");
                    explanation.availability = match availability.readiness {
                        TeamReadiness::Ready | TeamReadiness::Resumable => TeamActionAvailability::Available,
                        TeamReadiness::Held => TeamActionAvailability::Blocked,
                        TeamReadiness::Busy => TeamActionAvailability::Unconfirmed,
                        TeamReadiness::Unknown => TeamActionAvailability::Unconfirmed,
                    };
                    if explanation.reason.is_empty() {
                        explanation.reason = format!("Observed {:?}; delivery follows the requested boundary and the worker rechecks admission.", availability.activity);
                    }
                    if availability.readiness == TeamReadiness::Busy {
                        explanation.reason = "The agent is busy. Next-turn input waits; safe-boundary input may steer active work after the worker rechecks admission.".into();
                    }
                    if availability.readiness == TeamReadiness::Held {
                        explanation.resolution = Some("Resolve the listed hold through its owning control. Resume releases the global hold only; it grants no tool permissions.".into());
                    }

                }
            } else {
                explanation.availability = TeamActionAvailability::Blocked;
                explanation.reason = "Select an enrolled teammate.".into();
            }
        }
        TeamExplainableAction::Resume => {
            explanation.reason = "Resume releases the global team hold. Manual holds, pending approvals, and tool permissions retain their existing owners.".into();
        }
        TeamExplainableAction::Pause => explanation.reason = "Pause holds new execution. Already admitted work may finish; offline hosts remain unconfirmed.".into(),
        TeamExplainableAction::Stop => explanation.reason = "Stop interrupts enrolled work and requests cancellation through existing adapters; unconfirmed cancellation remains visible.".into(),
    }
    explanation
}
