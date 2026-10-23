//! Durable teammate administration is driven by coordinator notifications.

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::model::*;

impl Coordinator {
    pub(crate) async fn administration_operations(
        &self,
    ) -> Result<Vec<TeamAdministrationOperation>> {
        Ok(self
            .store
            .list("administration", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, operation)| operation)
            .collect())
    }

    pub(crate) async fn prepare_administration(
        &self,
        operation: TeamAdministrationOperation,
    ) -> Result<crate::board::BoardWrites> {
        ensure!(
            operation.state == TeamAdministrationState::Queued
                && operation.result.is_none()
                && operation.waiting_reason.is_none(),
            "new administration operation must be queued"
        );
        ensure!(
            self.store
                .get::<Agent>("agent", &address_key(&operation.actor))
                .await?
                .is_some(),
            "administration actor is not enrolled"
        );
        ensure!(
            uuid::Uuid::parse_str(&operation.operation_id).is_ok(),
            "invalid administration operation identity"
        );
        ensure!(
            self.store
                .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
                .await?
                .is_none(),
            "administration operation identity is already in use"
        );
        Ok(vec![(
            "administration",
            operation.operation_id.clone(),
            serde_json::to_value(operation)?,
        )])
    }

    pub(crate) async fn advance_administration(&self) -> Result<()> {
        for mut operation in self.administration_operations().await? {
            if matches!(
                operation.state,
                TeamAdministrationState::Succeeded | TeamAdministrationState::Rejected
            ) {
                continue;
            }
            // Publish ownership before a potentially slow worker RPC. A queued
            // receipt never masquerades as delivery or finished administration.
            if operation.state == TeamAdministrationState::Queued {
                operation.state = TeamAdministrationState::Running;
                self.store
                    .put("administration", &operation.operation_id, &operation)
                    .await?;
                self.notify();
            }
            let previous = operation.clone();
            match self.apply_administration(&operation).await {
                Ok((state, result, reason)) => {
                    operation.state = state;
                    operation.result = result;
                    operation.waiting_reason = reason;
                }
                Err(error) => {
                    operation.state = TeamAdministrationState::Rejected;
                    operation.waiting_reason = Some(error.to_string());
                }
            }
            if operation != previous {
                self.store
                    .put("administration", &operation.operation_id, &operation)
                    .await?;
                self.notify();
            }
        }
        Ok(())
    }

    async fn apply_administration(
        &self,
        operation: &TeamAdministrationOperation,
    ) -> Result<(
        TeamAdministrationState,
        Option<serde_json::Value>,
        Option<String>,
    )> {
        let request =
            match &operation.request {
                TeamAdministrationRequest::Member(TeamMemberActionParams::Remove { address }) => {
                    let retirement = self
                        .retire_member(resolved_address(address)?.clone(), &operation.operation_id)
                        .await?;
                    return Ok((
                        if retirement.state == TeamMemberRetirementState::Removed {
                            TeamAdministrationState::Succeeded
                        } else {
                            TeamAdministrationState::Waiting
                        },
                        Some(serde_json::to_value(&retirement)?),
                        retirement.waiting_reason,
                    ));
                }
                TeamAdministrationRequest::Member(TeamMemberActionParams::Create {
                    machine_id,
                    server_id,
                    alias,
                    cwd,
                }) => TeamRequest::CreateMember {
                    request: crate::TeamMemberCreateRequest {
                        machine_id: machine_id.clone(),
                        server_id: server_id.clone(),
                        alias: alias.clone(),
                        cwd: cwd.clone(),
                        model: None,
                        effort: None,
                    },
                },
                TeamAdministrationRequest::Member(TeamMemberActionParams::Enroll {
                    address,
                    alias,
                }) => {
                    ensure!(
                        self.manifest.machines.iter().any(|machine| machine.id
                            == address.machine_id
                            && machine
                                .servers
                                .iter()
                                .any(|server| server.id == address.server_id)),
                        "machine/server is not in the manifest"
                    );
                    if self
                        .peer(&address.machine_id, &address.server_id)
                        .await
                        .is_err()
                    {
                        return Ok((
                            TeamAdministrationState::Waiting,
                            None,
                            Some("Worker is disconnected; enrollment is pending.".into()),
                        ));
                    }
                    TeamRequest::Enroll {
                        address: address.clone(),
                        alias: alias.clone(),
                    }
                }
                TeamAdministrationRequest::Member(TeamMemberActionParams::Rename {
                    address,
                    alias,
                }) => {
                    let address = resolved_address(address)?;
                    ensure!(
                        self.store
                            .get::<Agent>("agent", &address_key(address))
                            .await?
                            .is_some(),
                        "member to rename is not enrolled"
                    );
                    if self
                        .peer(&address.machine_id, &address.server_id)
                        .await
                        .is_err()
                    {
                        return Ok((
                            TeamAdministrationState::Waiting,
                            None,
                            Some("Worker is disconnected; rename is pending.".into()),
                        ));
                    }
                    TeamRequest::Enroll {
                        address: address.clone(),
                        alias: alias.clone(),
                    }
                }
                TeamAdministrationRequest::Cleanup(TeamCleanupParams::Preview { scope }) => {
                    TeamRequest::BoardPurgePreview {
                        scope: scope.clone(),
                    }
                }
                TeamAdministrationRequest::Cleanup(TeamCleanupParams::Apply {
                    scope,
                    expected_revision,
                }) => TeamRequest::BoardPurge {
                    scope: scope.clone(),
                    expected_revision: *expected_revision,
                },
                TeamAdministrationRequest::Member(TeamMemberActionParams::Status { .. })
                | TeamAdministrationRequest::Cleanup(TeamCleanupParams::Status { .. }) => {
                    anyhow::bail!("status is a local read, not queued administration")
                }
            };
        let response = self
            .execute_operation(TeamOperation {
                operation_id: operation.operation_id.clone(),
                request,
            })
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error)
                if error
                    .downcast_ref::<codex_app_server_client::TypedRequestError>()
                    .is_some_and(|error| {
                        !matches!(
                            error,
                            codex_app_server_client::TypedRequestError::Server { .. }
                        )
                    }) =>
            {
                return Ok((
                    TeamAdministrationState::Waiting,
                    None,
                    Some(format!("Awaiting a confirmed worker response: {error}")),
                ));
            }
            Err(error) => return Err(error),
        };
        let (state, reason) = match &response {
            TeamResponse::Operation {
                operation: crate::TeamOperatorOperation::MemberCreate(created),
            } => match created.phase {
                crate::TeamMemberCreateState::Created => (TeamAdministrationState::Succeeded, None),
                crate::TeamMemberCreateState::Failed => {
                    (TeamAdministrationState::Rejected, created.error.clone())
                }
                crate::TeamMemberCreateState::Creating
                | crate::TeamMemberCreateState::EnrollmentPending
                | crate::TeamMemberCreateState::NeedsInspection => (
                    TeamAdministrationState::Waiting,
                    created
                        .error
                        .clone()
                        .or(Some("Member creation is pending.".into())),
                ),
            },
            TeamResponse::Operation {
                operation: crate::TeamOperatorOperation::BoardPurge(purged),
            } if purged.workers.iter().any(|worker| !worker.acknowledged) => (
                TeamAdministrationState::Waiting,
                Some("Board removed; waiting for worker cache acknowledgements.".into()),
            ),
            _ => (TeamAdministrationState::Succeeded, None),
        };
        Ok((state, Some(serde_json::to_value(response)?), reason))
    }
}

fn resolved_address(target: &TeamAgentTarget) -> Result<&TeamAddress> {
    match target {
        TeamAgentTarget::Address(address) => Ok(address),
        TeamAgentTarget::Alias(_) => {
            anyhow::bail!("administration target must be resolved before durable intake")
        }
    }
}
