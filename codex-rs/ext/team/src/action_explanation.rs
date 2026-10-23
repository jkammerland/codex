use crate::TeamHostService;
use anyhow::Result;
use codex_app_server_protocol::*;

impl TeamHostService {
    pub(crate) async fn agent_action_explanation(
        &self,
        sender: &TeamAddress,
        query: TeamExplainActionParams,
    ) -> Result<TeamActionExplanation> {
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let target = query.target.clone().unwrap_or_else(|| sender.clone());
        let directory = store
            .get::<TeamDirectorySnapshot>("directory_cache", "current")
            .await?
            .unwrap_or_default();
        let member = directory
            .data
            .iter()
            .find(|member| member.address == target);
        let mut result = TeamActionExplanation {
            action: query.action,
            target: Some(target),
            availability: TeamActionAvailability::Available,
            reason: "This agent can queue the action; the coordinator validates it at intake."
                .into(),
            owner: "Team coordinator".into(),
            resolution: None,
            freshness: TeamObservationFreshness::LastSynchronized,
        };
        match query.action {
            TeamExplainableAction::SelectModel
            | TeamExplainableAction::Pause
            | TeamExplainableAction::Resume
            | TeamExplainableAction::Stop => {
                result.availability = TeamActionAvailability::Blocked;
                result.reason = "This control belongs to the user/operator and is not exposed to the agent team tool.".into();
                result.resolution = Some("The user can apply the existing control in /agents or the operator CLI. Tool approval does not grant this authority.".into());
                result.freshness = TeamObservationFreshness::Current;
            }
            TeamExplainableAction::Enroll => {
                result.reason = "Team members can create or enroll teammates; new members inherit user-configured model and effort.".into();
            }
            TeamExplainableAction::Leave => {
                result.reason = "Use member/remove to retire a teammate. New work is rejected; admitted work drains before removal. Offline or held workers wait for the existing restriction to be resolved.".into();
            }
            TeamExplainableAction::ProcessMessages => {
                result.owner = "Selected worker host".into();
                if let Some(member) = member {
                    let availability = &member.discovery.availability;
                    result.reason = availability.reasons.join("; ");
                    result.availability = match availability.readiness {
                        TeamReadiness::Held => TeamActionAvailability::Blocked,
                        TeamReadiness::Busy => TeamActionAvailability::Unconfirmed,
                        TeamReadiness::Ready
                        | TeamReadiness::Resumable
                        | TeamReadiness::Unknown => TeamActionAvailability::Unconfirmed,
                    };
                    if availability.readiness == TeamReadiness::Busy {
                        result.reason = "The agent is busy. Next-turn input waits; safe-boundary input may steer active work after the worker rechecks admission.".into();
                    }
                    if result.reason.is_empty() {
                        result.reason = "Cached readiness is not an execution reservation; the worker rechecks admission.".into();
                    }
                } else {
                    result.availability = TeamActionAvailability::Unconfirmed;
                    result.reason =
                        "No synchronized enrollment observation is available for this task.".into();
                }
            }
            TeamExplainableAction::QueueMessage => {
                result.reason = "Messages may queue while the recipient is busy, offline, or held. Queueing does not admit a turn.".into();
                if member.is_none() {
                    result.availability = TeamActionAvailability::Unconfirmed;
                    result.reason = "Recipient enrollment is not present in the synchronized directory; intake will validate it.".into();
                }
            }
            TeamExplainableAction::Subscribe
                if query.target.as_ref().is_some_and(|target| target != sender) =>
            {
                result.availability = TeamActionAvailability::Blocked;
                result.reason =
                    "Agent credentials can change subscriptions only for their bound identity."
                        .into();
                result.freshness = TeamObservationFreshness::Current;
            }
            TeamExplainableAction::Post | TeamExplainableAction::Subscribe => {
                result.reason = "Board changes are queued as this agent. Subscription changes do not grant execution or tool permissions.".into();
            }
        }
        Ok(result)
    }
}
