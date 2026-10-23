use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use crate::coordinator::Coordinator;
use crate::model::*;
use crate::tests::agent;
use crate::tests::manifest;

async fn queued(
    coordinator: &Coordinator,
    actor: &TeamAddress,
    request: TeamAdministrationRequest,
) -> Result<TeamAdministrationOperation> {
    let operation = TeamAdministrationOperation {
        operation_id: uuid::Uuid::new_v4().to_string(),
        actor: actor.clone(),
        request,
        state: TeamAdministrationState::Queued,
        waiting_reason: None,
        result: None,
    };
    let writes = coordinator
        .prepare_administration(operation.clone())
        .await?;
    coordinator
        .store
        .put_records(&crate::board::borrowed_writes(&writes))
        .await?;
    Ok(operation)
}

async fn post(coordinator: &Coordinator, body: &str) -> Result<String> {
    let response = coordinator
        .execute_operation(TeamOperation::new(TeamRequest::Board {
            request: TeamBoardParams::Post {
                topic: "work".into(),
                body: body.into(),
                mentions: Vec::new(),
            },
        }))
        .await?;
    let TeamResponse::Board {
        response:
            TeamBoardResponse::Mutation {
                conversation_id: Some(id),
                ..
            },
    } = response
    else {
        panic!("expected conversation");
    };
    Ok(id)
}

#[tokio::test]
async fn retirement_rejects_new_work_keeps_visible_members_and_terminalizes_unadmitted_mail()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let configuration = manifest(directory.path());
    let coordinator = Coordinator::open(configuration.clone()).await?;
    let address = agent(&coordinator, "Finisher").await?;
    let pending = coordinator
        .operator_message(
            Some(address.clone()),
            "queued before removal".into(),
            TeamMessageKind::Request,
            None,
            None,
        )
        .await?;
    coordinator
        .ingest(pending.clone(), TeamDeliveryMode::NextTurn)
        .await?;
    let operation = queued(
        &coordinator,
        &address,
        TeamAdministrationRequest::Member(TeamMemberActionParams::Remove {
            address: address.clone().into(),
        }),
    )
    .await?;
    coordinator.advance_administration().await?;
    let state = coordinator.administration_operations().await?.remove(0);
    assert_eq!(state.state, TeamAdministrationState::Waiting);
    let agents = coordinator.agents().await?;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].status, "retiring");
    assert!(
        agents[0]
            .discovery
            .availability
            .reasons
            .iter()
            .any(|reason| reason.contains("Retiring"))
    );
    let later = coordinator
        .operator_message(
            Some(address.clone()),
            "new work".into(),
            TeamMessageKind::Request,
            None,
            None,
        )
        .await?;
    assert!(
        coordinator
            .ingest(later, TeamDeliveryMode::NextTurn)
            .await
            .unwrap_err()
            .to_string()
            .contains("retiring")
    );

    // Restart preserves the intent without needing an execution Stop/Resume.
    drop(coordinator);
    let coordinator = Coordinator::open(configuration).await?;
    assert!(coordinator.member_is_retiring(&address).await?);
    let mut removed = coordinator
        .retirement_intents(&address.machine_id, &address.server_id)
        .await?
        .remove(0);
    removed.state = TeamMemberRetirementState::Removed;
    removed.waiting_reason = None;
    coordinator
        .observe_retirements(&address.machine_id, &address.server_id, &[removed.clone()])
        .await?;
    coordinator
        .observe_retirements(&address.machine_id, &address.server_id, &[removed])
        .await?;
    coordinator.advance_administration().await?;
    assert!(coordinator.agents().await?.is_empty());
    let result = coordinator
        .store
        .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
        .await?
        .unwrap();
    assert_eq!(result.state, TeamAdministrationState::Succeeded);
    assert_eq!(
        coordinator
            .operator_operation(&operation.operation_id)
            .await?,
        crate::TeamOperatorOperation::Administration(result)
    );
    let message = coordinator
        .store
        .get::<MessageRecord>("message", &pending.id)
        .await?
        .unwrap();
    assert_eq!(message.receipts.len(), 1);
    let receipt = message.receipts.values().next().unwrap();
    assert_eq!(receipt.state, TeamReceiptState::Rejected);
    assert!(receipt.detail.as_deref().unwrap().contains("removed"));
    Ok(())
}

#[tokio::test]
async fn retiring_agents_can_publish_results_but_cannot_receive_mentions_or_subscription_wakes()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let address = agent(&coordinator, "Finisher").await?;
    coordinator
        .subscribe(address.clone(), "work".into(), true)
        .await?;
    let conversation = post(&coordinator, "Existing work").await?;
    let identity = TeamBoardIdentity::Agent {
        address: address.clone(),
    };
    let (_, writes) = coordinator
        .prepare_board_request(
            TeamBoardParams::Follow {
                conversation_id: conversation.clone(),
                identity: None,
                enabled: true,
            },
            identity.clone(),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await?;
    coordinator
        .store
        .put_records(&crate::board::borrowed_writes(&writes))
        .await?;
    coordinator
        .retire_member(address.clone(), &uuid::Uuid::new_v4().to_string())
        .await?;
    let new_post = post(&coordinator, "Not new work for retirees").await?;
    assert_eq!(
        coordinator
            .store
            .get::<MessageRecord>("message", &new_post)
            .await?
            .unwrap()
            .targets,
        Some(Vec::new())
    );
    let reply_id = uuid::Uuid::new_v4().to_string();
    let (_, writes) = coordinator
        .prepare_board_request(
            TeamBoardParams::Reply {
                conversation_id: conversation.clone(),
                reply_to: None,
                body: "Final result from admitted turn".into(),
                mentions: Vec::new(),
            },
            identity,
            &reply_id,
        )
        .await?;
    coordinator
        .store
        .put_records(&crate::board::borrowed_writes(&writes))
        .await?;
    assert_eq!(
        coordinator
            .store
            .get::<MessageRecord>("message", &reply_id)
            .await?
            .unwrap()
            .targets,
        Some(Vec::new())
    );
    let error = coordinator
        .prepare_board_request(
            TeamBoardParams::Reply {
                conversation_id: conversation,
                reply_to: None,
                body: "@Finisher more work".into(),
                mentions: vec![TeamBoardMention {
                    identity: TeamBoardIdentity::Agent { address },
                    display: "@Finisher".into(),
                    start: 0,
                    end: 9,
                }],
            },
            TeamBoardIdentity::User,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retiring"), "{error}");
    Ok(())
}

#[tokio::test]
async fn agent_cleanup_reuses_online_purge_with_attribution_tombstones_and_idempotent_status()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let configuration = manifest(directory.path());
    let coordinator = Coordinator::open(configuration.clone()).await?;
    let actor = agent(&coordinator, "Maintainer").await?;
    let conversation = post(&coordinator, "Remove this").await?;
    let scope = TeamBoardPurgeScope::Conversation {
        conversation_id: conversation.clone(),
    };
    let preview = queued(
        &coordinator,
        &actor,
        TeamAdministrationRequest::Cleanup(TeamCleanupParams::Preview {
            scope: scope.clone(),
        }),
    )
    .await?;
    coordinator.advance_administration().await?;
    let preview = coordinator
        .store
        .get::<TeamAdministrationOperation>("administration", &preview.operation_id)
        .await?
        .unwrap();
    assert_eq!(preview.state, TeamAdministrationState::Succeeded);
    let response: TeamResponse = serde_json::from_value(preview.result.unwrap())?;
    let TeamResponse::BoardPurgePreview { preview } = response else {
        panic!("expected purge preview");
    };
    assert_eq!((preview.conversation_count, preview.message_count), (1, 1));
    let operation = queued(
        &coordinator,
        &actor,
        TeamAdministrationRequest::Cleanup(TeamCleanupParams::Apply {
            scope,
            expected_revision: preview.revision,
        }),
    )
    .await?;
    coordinator.advance_administration().await?;
    let completed = coordinator
        .store
        .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
        .await?
        .unwrap();
    assert_eq!(completed.actor, actor);
    assert_eq!(completed.state, TeamAdministrationState::Succeeded);
    let operations = coordinator.operator_operations().await?;
    assert_eq!(operations.len(), 2);
    assert!(
        operations
            .iter()
            .all(|operation| matches!(operation, crate::TeamOperatorOperation::Administration(_)))
    );
    assert!(
        coordinator
            .store
            .board_message_removed(&conversation)
            .await?
    );
    assert!(
        coordinator
            .store
            .get::<TeamBoardEntry>("board_entry", &conversation)
            .await?
            .is_none()
    );
    drop(coordinator);
    let coordinator = Coordinator::open(configuration).await?;
    coordinator.advance_administration().await?;
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
            .await?,
        Some(completed)
    );
    assert!(
        coordinator
            .store
            .board_message_removed(&conversation)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn intake_rejection_is_terminal_and_duplicate_delivery_keeps_the_same_actor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let actor = agent(&coordinator, "Departing").await?;
    let machine = crate::Machine {
        id: actor.machine_id.clone(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        id: actor.server_id.clone(),
        label: "Account".into(),
        endpoint: crate::Endpoint::Local {
            socket: directory.path().join("socket"),
        },
        cancellation_adapters: Vec::new(),
        environment_capabilities: Default::default(),
    };
    coordinator
        .store
        .put(
            "installation",
            &server_key(&machine.id, &server.id),
            &"installation",
        )
        .await?;
    let operation = TeamAdministrationOperation {
        operation_id: uuid::Uuid::new_v4().to_string(),
        actor: actor.clone(),
        request: TeamAdministrationRequest::Cleanup(TeamCleanupParams::Preview {
            scope: TeamBoardPurgeScope::All,
        }),
        state: TeamAdministrationState::Queued,
        waiting_reason: None,
        result: None,
    };
    let entry = TeamOutboxEntry {
        sequence: 1,
        id: operation.operation_id.clone(),
        kind: "administration".into(),
        payload: serde_json::to_value(&operation)?,
    };
    coordinator
        .store
        .delete("agent", &address_key(&actor))
        .await?;
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    let rejected = coordinator
        .store
        .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
        .await?
        .unwrap();
    assert_eq!(
        rejected,
        TeamAdministrationOperation {
            state: TeamAdministrationState::Rejected,
            waiting_reason: Some("administration actor is not enrolled".into()),
            ..operation
        }
    );
    assert_eq!(
        coordinator
            .store
            .list::<RejectedOutbox>("rejected_outbox", "", u32::MAX)
            .await?
            .len(),
        1
    );
    coordinator.advance_administration().await?;
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &rejected.operation_id)
            .await?,
        Some(rejected)
    );
    Ok(())
}

#[tokio::test]
async fn disconnected_creation_retries_preserve_pending_state_and_inherited_model_policy()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut configuration = manifest(directory.path());
    configuration.machines.push(crate::Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![crate::Server {
            id: "account".into(),
            label: "Account".into(),
            endpoint: crate::Endpoint::Local {
                socket: directory.path().join("unavailable.sock"),
            },
            cancellation_adapters: Vec::new(),
            environment_capabilities: Default::default(),
        }],
    });
    let coordinator = Coordinator::open(configuration.clone()).await?;
    let actor = agent(&coordinator, "Organizer").await?;
    let operation = queued(
        &coordinator,
        &actor,
        TeamAdministrationRequest::Member(TeamMemberActionParams::Create {
            machine_id: "worker".into(),
            server_id: "account".into(),
            alias: "Builder".into(),
            cwd: "/checkout".into(),
        }),
    )
    .await?;
    coordinator.advance_administration().await?;
    let pending = coordinator
        .store
        .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
        .await?
        .unwrap();
    assert_eq!(pending.state, TeamAdministrationState::Waiting);
    coordinator.advance_administration().await?;
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
            .await?,
        Some(pending.clone())
    );
    drop(coordinator);
    let restored = Coordinator::open(configuration).await?;
    restored.advance_administration().await?;
    assert_eq!(
        restored
            .store
            .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
            .await?,
        Some(pending)
    );
    let mut creation = restored
        .store
        .get::<crate::TeamMemberCreateOperation>("member_create", &operation.operation_id)
        .await?
        .unwrap();
    assert_eq!(
        (
            creation.request.model.clone(),
            creation.request.effort.clone()
        ),
        (None, None)
    );
    assert_eq!(creation.phase, crate::TeamMemberCreateState::Creating);
    creation.phase = crate::TeamMemberCreateState::EnrollmentPending;
    restored
        .store
        .put("member_create", &operation.operation_id, &creation)
        .await?;
    restored.advance_administration().await?;
    assert_eq!(
        restored
            .store
            .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
            .await?
            .unwrap()
            .state,
        TeamAdministrationState::Waiting
    );
    creation.phase = crate::TeamMemberCreateState::Created;
    restored
        .store
        .put("member_create", &operation.operation_id, &creation)
        .await?;
    restored.advance_administration().await?;
    assert_eq!(
        restored
            .store
            .get::<TeamAdministrationOperation>("administration", &operation.operation_id)
            .await?
            .unwrap()
            .state,
        TeamAdministrationState::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn distinct_concurrent_removal_is_rejected_while_original_retry_remains_idempotent()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let address = agent(&coordinator, "Finisher").await?;
    let request = TeamAdministrationRequest::Member(TeamMemberActionParams::Remove {
        address: address.clone().into(),
    });
    let first = queued(&coordinator, &address, request.clone()).await?;
    coordinator.advance_administration().await?;
    let retiring = coordinator
        .retirement_intents(&address.machine_id, &address.server_id)
        .await?
        .remove(0);
    assert_eq!(
        coordinator
            .retire_member(address.clone(), &first.operation_id)
            .await?,
        retiring
    );

    let second = queued(&coordinator, &address, request).await?;
    assert_ne!(first.operation_id, second.operation_id);
    coordinator.advance_administration().await?;
    let rejected = TeamAdministrationOperation {
        state: TeamAdministrationState::Rejected,
        waiting_reason: Some(format!(
            "member is already retiring under operation {}",
            first.operation_id
        )),
        ..second
    };
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &rejected.operation_id)
            .await?,
        Some(rejected.clone())
    );
    assert_eq!(
        coordinator
            .retirement_intents(&address.machine_id, &address.server_id)
            .await?,
        vec![retiring.clone()]
    );

    let removed = TeamMemberRetirement {
        state: TeamMemberRetirementState::Removed,
        waiting_reason: None,
        ..retiring
    };
    coordinator
        .observe_retirements(
            &address.machine_id,
            &address.server_id,
            std::slice::from_ref(&removed),
        )
        .await?;
    coordinator.advance_administration().await?;
    assert_eq!(
        coordinator
            .retire_member(address, &first.operation_id)
            .await?,
        removed
    );
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &first.operation_id)
            .await?
            .unwrap()
            .state,
        TeamAdministrationState::Succeeded
    );
    assert_eq!(
        coordinator
            .store
            .get::<TeamAdministrationOperation>("administration", &rejected.operation_id)
            .await?,
        Some(rejected)
    );
    Ok(())
}
