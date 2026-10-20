//! Per-field ownership governs inheritance; equal values never establish intent.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RequestedSelection {
    selection: TeamModelSelection,
    model: Option<String>,
    effort: Option<String>,
}

impl Coordinator {
    pub(crate) async fn select_model(
        &self,
        address: TeamAddress,
        model: TeamSelection,
        effort: TeamSelection,
    ) -> Result<Agent> {
        self.select_model_operation(address, model, effort, None)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serializes destination selection against leave and inheritance; intent persistence and snapshot fencing share the mutation lock"
    )]
    pub(crate) async fn select_model_operation(
        &self,
        address: TeamAddress,
        model: TeamSelection,
        effort: TeamSelection,
        operation: Option<(&str, &str)>,
    ) -> Result<Agent> {
        let address = canonical_address(address)?;
        let lock = self
            .model_update_lock(&address.machine_id, &address.server_id)
            .await;
        let _selection = lock.lock().await;
        let key = address_key(&address);
        let agent = self
            .store
            .get::<Agent>("agent", &key)
            .await?
            .context("agent is not enrolled")?;
        let models = self.models(&address.machine_id, &address.server_id).await?;
        validate_selection(&agent, &models, &model, &effort)?;
        let agents = self.agents().await?;
        let desired = resolve_selection(&agent, &agents, model, effort)?;
        let _mutation = self.mutations.lock().await;
        if let Some((id, digest)) = operation
            && let Some(TeamResponse::Agent { agent }) = self.operation_result(id, digest).await?
        {
            return Ok(*agent);
        }
        let agent = self
            .store
            .get::<Agent>("agent", &key)
            .await?
            .context("agent is no longer enrolled")?;
        self.persist_selection(agent, desired, operation).await
    }

    async fn persist_selection(
        &self,
        mut agent: Agent,
        mut desired: RequestedSelection,
        operation: Option<(&str, &str)>,
    ) -> Result<Agent> {
        let key = address_key(&agent.enrollment.address);
        let previous = self
            .store
            .get::<RequestedSelection>("model_request", &key)
            .await?;
        if let Some(previous) = previous.as_ref().filter(|previous| {
            agent
                .runtime
                .as_ref()
                .into_iter()
                .flat_map(|runtime| {
                    runtime
                        .applied_selection_revision
                        .into_iter()
                        .chain(runtime.superseded_selection_revision)
                })
                .max()
                .is_none_or(|revision| revision < previous.selection.revision)
        }) {
            // A sparse edit must preserve earlier unacknowledged ownership,
            // including inheritance that has not reached the worker yet.
            if desired.selection.model == TeamSelection::Keep {
                desired.selection.model = previous.selection.model.clone();
                desired.model.clone_from(&previous.model);
            }
            if desired.selection.effort == TeamSelection::Keep {
                desired.selection.effort = previous.selection.effort.clone();
                desired.effort.clone_from(&previous.effort);
            }
        }
        desired.selection.revision = [
            previous.as_ref().map(|request| request.selection.revision),
            agent
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.requested_selection_revision),
            agent
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.applied_selection_revision),
            agent
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.superseded_selection_revision),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .context("model selection revision exhausted")?;
        agent.requested_selection = Some(desired.selection.clone());
        let response = TeamResponse::Agent {
            agent: Box::new(agent.clone()),
        };
        let generated = Uuid::now_v7().to_string();
        let (receipt, id, digest) = operation.map_or(
            (
                "model_request_revision",
                generated.as_str(),
                generated.as_str(),
            ),
            |(id, digest)| ("operator_operation", id, digest),
        );
        self.store
            .put_record_once(
                receipt,
                id,
                digest,
                &[
                    ("model_request", &key, serde_json::to_value(&desired)?),
                    ("agent", &key, serde_json::to_value(&agent)?),
                    ("operator_result", id, serde_json::to_value(response)?),
                ],
            )
            .await?;
        *self
            .observed_versions
            .lock()
            .await
            .entry(server_key(
                &agent.enrollment.address.machine_id,
                &agent.enrollment.address.server_id,
            ))
            .or_default() += 1;
        self.notify();
        Ok(agent)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "keeps each destination's selection RPCs ordered"
    )]
    pub(crate) async fn inherit_models(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        let lock = self.model_update_lock(&machine.id, &server.id).await;
        let _selection = lock.lock().await;
        for child in self.agents().await? {
            let address = &child.enrollment.address;
            if address.machine_id != machine.id
                || address.server_id != server.id
                || child
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.archived)
            {
                continue;
            }
            self.apply_model_selection(address, peer).await?;
        }
        Ok(())
    }

    /// The caller holds the destination's model-update lock through dispatch.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "resolves and journals inherited intent from one snapshot under the mutation fence"
    )]
    pub(crate) async fn apply_model_selection(
        &self,
        address: &TeamAddress,
        peer: &Peer,
    ) -> Result<()> {
        let request = {
            // Resolve from current state atomically so notifications cannot
            // leave inheritance based on an older snapshot before first wake.
            let _mutation = self.mutations.lock().await;
            let agents = self.agents().await?;
            let Some(child) = agents
                .iter()
                .find(|agent| agent.enrollment.address == *address)
            else {
                return Ok(());
            };
            let Some(runtime) = child.runtime.as_ref() else {
                return Ok(());
            };
            if runtime.archived {
                return Ok(());
            }
            let key = address_key(address);
            let pending = self
                .store
                .get::<RequestedSelection>("model_request", &key)
                .await?;
            if let Some(pending) = pending.clone().filter(|pending| {
                runtime
                    .applied_selection_revision
                    .into_iter()
                    .chain(runtime.superseded_selection_revision)
                    .max()
                    .is_none_or(|revision| revision < pending.selection.revision)
            }) {
                // An unloaded inherited request can remain pending while its
                // parent changes. Refresh those fields before first wake;
                // explicit fields retain the operator's selected values.
                let desired = resolve_selection(
                    child,
                    &agents,
                    pending.selection.model.clone(),
                    pending.selection.effort.clone(),
                )?;
                if desired.model != pending.model || desired.effort != pending.effort {
                    self.persist_selection(child.clone(), desired, None).await?;
                    self.store
                        .get::<RequestedSelection>("model_request", &key)
                        .await?
                } else {
                    Some(pending)
                }
            } else {
                // Unloaded snapshots may predate a local edit. Only replay
                // actual pending choices until readiness refreshes provenance.
                if !runtime.loaded {
                    return Ok(());
                }
                let (model, effort) = inherited_fields(runtime);
                if model == TeamSelection::Keep && effort == TeamSelection::Keep {
                    return Ok(());
                }
                let desired = resolve_selection(child, &agents, model, effort)?;
                if !requires_update(child, &desired)
                    || pending
                        .as_ref()
                        .is_some_and(|previous| same_inherited_intent(previous, &desired))
                {
                    return Ok(());
                }
                self.persist_selection(child.clone(), desired, None).await?;
                self.store
                    .get::<RequestedSelection>("model_request", &key)
                    .await?
            }
        };
        let Some(request) = request else {
            return Ok(());
        };
        // Queue acknowledgement does not establish next-turn application.
        // The host runtime's applied revision is the authoritative receipt.
        let _: ThreadSettingsUpdateResponse = peer
            .handle
            .request_typed(ClientRequest::ThreadSettingsUpdate {
                request_id: request_id(),
                params: ThreadSettingsUpdateParams {
                    thread_id: address.thread_id.clone(),
                    model: request.model,
                    effort: request
                        .effort
                        .as_deref()
                        .map(str::parse)
                        .transpose()
                        .map_err(anyhow::Error::msg)?,
                    team_selection: Some(request.selection),
                    ..Default::default()
                },
            })
            .await?;
        Ok(())
    }
}

fn inherited_fields(runtime: &TeamAgentRuntime) -> (TeamSelection, TeamSelection) {
    let field = |origin| {
        if origin == TeamSelectionOrigin::Inherited {
            TeamSelection::Inherit
        } else {
            TeamSelection::Keep
        }
    };
    (field(runtime.model_origin), field(runtime.effort_origin))
}

fn resolve_selection(
    agent: &Agent,
    agents: &[Agent],
    model: TeamSelection,
    effort: TeamSelection,
) -> Result<RequestedSelection> {
    let parent = agent
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.parent_thread_id.as_ref())
        .and_then(|parent| {
            agents.iter().find(|candidate| {
                candidate.enrollment.address.machine_id == agent.enrollment.address.machine_id
                    && candidate.enrollment.address.server_id == agent.enrollment.address.server_id
                    && candidate.enrollment.address.thread_id == *parent
            })
        });
    let inherited = || parent.context("parent settings are unavailable for inheritance");
    let resolved_model = match &model {
        TeamSelection::Value { value } => Some(value.clone()),
        TeamSelection::Inherit => inherited()?.next_model.clone(),
        TeamSelection::Keep | TeamSelection::Default => None,
    };
    let resolved_effort = match &effort {
        TeamSelection::Value { value } => Some(value.clone()),
        TeamSelection::Inherit => inherited()?.next_effort.clone(),
        TeamSelection::Keep | TeamSelection::Default => None,
    };
    Ok(RequestedSelection {
        selection: TeamModelSelection {
            revision: 0,
            model,
            effort,
        },
        model: resolved_model,
        effort: resolved_effort,
    })
}

fn same_inherited_intent(previous: &RequestedSelection, desired: &RequestedSelection) -> bool {
    let model = desired.selection.model == TeamSelection::Keep
        || previous.selection.model == TeamSelection::Inherit && previous.model == desired.model;
    let effort = desired.selection.effort == TeamSelection::Keep
        || previous.selection.effort == TeamSelection::Inherit && previous.effort == desired.effort;
    model && effort
}

fn requires_update(agent: &Agent, desired: &RequestedSelection) -> bool {
    desired.selection.model != TeamSelection::Keep && desired.model != agent.next_model
        || desired.selection.effort != TeamSelection::Keep && desired.effort != agent.next_effort
}

fn validate_selection(
    agent: &Agent,
    models: &[Model],
    model: &TeamSelection,
    effort: &TeamSelection,
) -> Result<()> {
    let selected = match model {
        TeamSelection::Value { value } => {
            Some(models.iter().find(|model| model.model == *value).context(
                "model is unavailable on this host; select a supported model explicitly",
            )?)
        }
        TeamSelection::Keep => agent
            .next_model
            .as_ref()
            .and_then(|value| models.iter().find(|model| model.model == *value)),
        TeamSelection::Default | TeamSelection::Inherit => None,
    };
    if let TeamSelection::Value { value } = effort {
        let selected =
            selected.context("select the host model explicitly when setting its effort")?;
        ensure!(
            selected
                .supported_reasoning_efforts
                .iter()
                .any(|effort| effort.reasoning_effort.to_string() == *value),
            "effort is not supported by this model"
        );
    } else if let Some(selected) = selected {
        ensure!(
            effort != &TeamSelection::Default
                || selected.supported_reasoning_efforts.is_empty()
                || selected.default_reasoning_effort.to_string() != "ultra",
            "Ultra requires an explicit effort selection"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "model_policy_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "model_policy_delivery_tests.rs"]
mod delivery_tests;
