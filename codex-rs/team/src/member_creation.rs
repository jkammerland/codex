//! Coordinate creation without holding the shared mutation fence across host I/O.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::TeamMemberCreateOperation;
use crate::TeamMemberCreateRequest;
use crate::TeamMemberCreateState;
use crate::TeamOperatorOperation;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::server_key;

impl Coordinator {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "creation intent and alias claims commit in one mutation order"
    )]
    pub(crate) async fn create_member(
        &self,
        operation_id: &str,
        digest: &str,
        request: TeamMemberCreateRequest,
    ) -> Result<TeamOperatorOperation> {
        let _reservation = self.enrollments.reserve(request.alias.clone())?;
        let _mutation = self.mutations.lock().await;
        if let Some(previous) = self
            .store
            .get::<String>("member_create_digest", operation_id)
            .await?
        {
            ensure!(
                previous == digest,
                "operation identity has conflicting content"
            );
            return self.operator_operation(operation_id).await;
        }
        ensure!(
            self.store
                .get::<serde_json::Value>("operator_operation", operation_id)
                .await?
                .is_none(),
            "operation identity has conflicting content"
        );
        let validation: Result<()> = async {
            ensure!(
                !request.alias.trim().is_empty(),
                "an agent alias is required"
            );
            ensure!(
                !request.cwd.trim().is_empty(),
                "a checkout directory is required"
            );
            ensure!(
                self.manifest
                    .machines
                    .iter()
                    .any(|machine| machine.id == request.machine_id
                        && machine
                            .servers
                            .iter()
                            .any(|server| server.id == request.server_id)),
                "machine/server is not in the manifest"
            );
            // Deserialize using the host protocol's effort type before accepting
            // durable work, so an invalid option cannot become a reconnect loop.
            let _ = member_params(&request, operation_id, "", "")?;
            self.ensure_creation_alias_available(&request.alias, None)
                .await?;
            ensure!(
                !self
                    .agents()
                    .await?
                    .iter()
                    .any(|agent| agent.enrollment.alias == request.alias),
                "agent alias is already in use"
            );
            Ok(())
        }
        .await;
        if let Err(error) = validation {
            let rejection = crate::TeamOperationRejected {
                operation_id: operation_id.into(),
                reason: error.to_string(),
            };
            self.store
                .put_record_once(
                    "operator_operation",
                    operation_id,
                    digest,
                    &[(
                        "operator_rejection",
                        operation_id,
                        serde_json::to_value(&rejection)?,
                    )],
                )
                .await?;
            return Err(rejection.into());
        }
        let operation = TeamMemberCreateOperation {
            operation_id: operation_id.into(),
            request,
            phase: TeamMemberCreateState::Creating,
            address: None,
            error: None,
        };
        self.store
            .put_records(&[
                (
                    "member_create_digest",
                    operation_id,
                    serde_json::to_value(digest)?,
                ),
                (
                    "member_create",
                    operation_id,
                    serde_json::to_value(&operation)?,
                ),
                (
                    "operator_operation",
                    operation_id,
                    serde_json::from_str(digest)?,
                ),
                (
                    "operator_result",
                    operation_id,
                    serde_json::to_value(crate::TeamResponse::Operation {
                        operation: TeamOperatorOperation::MemberCreate(operation.clone()),
                    })?,
                ),
            ])
            .await?;
        self.notify();
        Ok(TeamOperatorOperation::MemberCreate(operation))
    }

    pub(crate) async fn ensure_creation_alias_available(
        &self,
        alias: &str,
        address: Option<&TeamAddress>,
    ) -> Result<()> {
        for (_, operation) in self
            .store
            .list::<TeamMemberCreateOperation>("member_create", "", u32::MAX)
            .await?
        {
            if matches!(
                operation.phase,
                TeamMemberCreateState::Created | TeamMemberCreateState::Failed
            ) {
                continue;
            }
            ensure!(
                operation.request.alias != alias
                    || address.is_some_and(|address| operation.address.as_ref() == Some(address)),
                "agent alias is reserved by a member creation operation"
            );
        }
        Ok(())
    }

    pub(crate) async fn operator_operation(
        &self,
        operation_id: &str,
    ) -> Result<TeamOperatorOperation> {
        if let Some(operation) = self.store.get("member_create", operation_id).await? {
            return Ok(TeamOperatorOperation::MemberCreate(operation));
        }
        self.store
            .board_purge_operation(operation_id)
            .await?
            .map(TeamOperatorOperation::BoardPurge)
            .context("operator operation not found")
    }

    pub(crate) async fn operator_operations(&self) -> Result<Vec<TeamOperatorOperation>> {
        let mut operations = self
            .store
            .list::<TeamMemberCreateOperation>("member_create", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, operation)| TeamOperatorOperation::MemberCreate(operation))
            .collect::<Vec<_>>();
        for journal in self.store.board_purge_journal().await? {
            if let Some(operation) = self
                .store
                .board_purge_operation(&journal.operation_id)
                .await?
            {
                operations.push(TeamOperatorOperation::BoardPurge(operation));
            }
        }
        Ok(operations)
    }

    pub(crate) async fn resume_member_creations(
        &self,
        machine_id: &str,
        server_id: &str,
        peer: &Peer,
    ) -> Result<()> {
        // Initial reconciliation precedes peer registration. Its notification
        // schedules this work after the same verified peer becomes attachable.
        if !self
            .peers
            .read()
            .await
            .contains_key(&server_key(machine_id, server_id))
        {
            return Ok(());
        }
        let installation_id = self
            .store
            .get::<String>("installation", &server_key(machine_id, server_id))
            .await?
            .context("worker installation is not verified")?;
        for (_, mut operation) in self
            .store
            .list::<TeamMemberCreateOperation>("member_create", "", u32::MAX)
            .await?
        {
            if operation.request.machine_id != machine_id
                || operation.request.server_id != server_id
                || !matches!(
                    operation.phase,
                    TeamMemberCreateState::Creating | TeamMemberCreateState::EnrollmentPending
                )
            {
                continue;
            }
            let previous = operation.clone();
            if operation.phase == TeamMemberCreateState::Creating {
                let params = member_params(
                    &operation.request,
                    &operation.operation_id,
                    &self.current_control().await?.coordinator_id,
                    &installation_id,
                )?;
                match peer
                    .handle
                    .request_typed::<TeamMemberCreateResponse>(ClientRequest::TeamMemberCreate {
                        request_id: request_id(),
                        params,
                    })
                    .await
                {
                    Ok(response) => {
                        ensure!(
                            response.operation_id == operation.operation_id,
                            "worker returned a different creation operation"
                        );
                        let thread_id = uuid::Uuid::parse_str(&response.thread_id)?.to_string();
                        operation.address = Some(TeamAddress {
                            machine_id: machine_id.into(),
                            server_id: server_id.into(),
                            thread_id,
                        });
                        operation.error = response.error;
                        operation.phase = match response.phase {
                            TeamMemberCreatePhase::Created => {
                                TeamMemberCreateState::EnrollmentPending
                            }
                            TeamMemberCreatePhase::Failed => TeamMemberCreateState::Failed,
                            TeamMemberCreatePhase::NeedsInspection => {
                                TeamMemberCreateState::NeedsInspection
                            }
                        };
                    }
                    Err(error) => {
                        operation.error = Some(format!(
                            "Creation is awaiting a confirmed worker response: {error}"
                        ))
                    }
                }
                self.save_member_creation(&previous, &operation).await?;
            }
            if operation.phase == TeamMemberCreateState::EnrollmentPending {
                let previous = operation.clone();
                let address = operation
                    .address
                    .clone()
                    .context("created member address is missing")?;
                let enrollment_id = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_OID,
                    format!("member-enroll:{}", operation.operation_id).as_bytes(),
                )
                .to_string();
                match self
                    .enroll(
                        address,
                        operation.request.alias.clone(),
                        Some(&enrollment_id),
                    )
                    .await
                {
                    Ok(_) => {
                        operation.phase = TeamMemberCreateState::Created;
                        operation.error = None;
                    }
                    Err(error) => {
                        operation.error =
                            Some(format!("Session created; enrollment is pending: {error}"))
                    }
                }
                self.save_member_creation(&previous, &operation).await?;
            }
        }
        Ok(())
    }

    async fn save_member_creation(
        &self,
        previous: &TeamMemberCreateOperation,
        operation: &TeamMemberCreateOperation,
    ) -> Result<()> {
        if previous != operation {
            self.store
                .put("member_create", &operation.operation_id, operation)
                .await?;
            self.notify();
        }
        Ok(())
    }
}

fn member_params(
    request: &TeamMemberCreateRequest,
    operation_id: &str,
    coordinator_id: &str,
    installation_id: &str,
) -> Result<TeamMemberCreateParams> {
    Ok(TeamMemberCreateParams {
        operation_id: operation_id.into(),
        coordinator_id: coordinator_id.into(),
        installation_id: installation_id.into(),
        cwd: request.cwd.clone(),
        model: request.model.clone(),
        effort: request
            .effort
            .as_ref()
            .map(|value| serde_json::from_value(serde_json::Value::String(value.clone())))
            .transpose()
            .context("invalid reasoning effort")?,
    })
}
