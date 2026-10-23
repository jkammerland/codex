use super::delivery;
use super::host;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::sync::Weak;

async fn enroll(
    service: &crate::TeamHostService,
) -> anyhow::Result<(ThreadId, TeamMemberRetirement)> {
    let root = ThreadId::new();
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: root.to_string(),
    };
    service
        .store
        .get()
        .unwrap()
        .enroll(
            &TeamEnrollment {
                address: address.clone(),
                alias: "worker".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            "credential",
        )
        .await?;
    service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 1,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    Ok((
        root,
        TeamMemberRetirement {
            operation_id: uuid::Uuid::new_v4().to_string(),
            address,
            state: TeamMemberRetirementState::Retiring,
            waiting_reason: None,
        },
    ))
}

#[tokio::test]
async fn retirement_waits_for_admitted_work_and_rejects_only_pending_deliveries()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let (root, intent) = enroll(&service).await?;
    let store = service.store.get().unwrap();
    let mut pending = delivery(&root.to_string());
    store.receive(&pending).await?;
    pending.id = "admitted".into();
    pending.message.id = "admitted-message".into();
    let admitted = TeamReceipt {
        state: TeamReceiptState::Accepted,
        turn_id: Some("turn".into()),
        ..store.receive(&pending).await?
    };
    store.update_receipt(&admitted).await?;
    let work = service.gate.try_enter(root).unwrap();
    service.apply_retirement(&intent).await?;
    service.advance_retirements().await?;
    assert_eq!(
        service.retirement_states().await?[0].state,
        TeamMemberRetirementState::Retiring
    );
    assert!(store.enrollment(&root.to_string()).await?.is_some());
    assert!(service.gate.try_start_turn(root).is_none());
    let receipts = store.deliveries(&root.to_string()).await?;
    assert_eq!(
        receipts
            .iter()
            .find(|(_, receipt)| receipt.delivery_id == "delivery")
            .unwrap()
            .1
            .state,
        TeamReceiptState::Rejected
    );
    assert_eq!(
        receipts
            .iter()
            .find(|(_, receipt)| receipt.delivery_id == "admitted")
            .unwrap()
            .1,
        admitted
    );

    drop(work);
    service.advance_retirements().await?;
    assert_eq!(
        service.retirement_states().await?[0].state,
        TeamMemberRetirementState::Retiring
    );
    store
        .update_receipt(&TeamReceipt {
            state: TeamReceiptState::Completed,
            ..admitted
        })
        .await?;
    service.advance_retirements().await?;
    assert_eq!(
        service.retirement_states().await?[0].state,
        TeamMemberRetirementState::Removed
    );
    assert!(store.enrollment(&root.to_string()).await?.is_none());
    assert_eq!(
        store.control().await?.unwrap().mode,
        TeamExecutionMode::Running
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_restart_restores_retirement_and_manual_hold_after_enrollment_deletion()
-> anyhow::Result<()> {
    let (home, service) = host().await?;
    let (root, intent) = enroll(&service).await?;
    service.apply_retirement(&intent).await?;
    service.gate.hold_manually(root);
    let store = service.store.get().unwrap();
    store.put("manual_hold", &root.to_string(), &true).await?;
    // Exercise the crash window after credential revocation and before the
    // final atomic gate check. The retirement row owns the surviving fence.
    store
        .unenroll(&root.to_string(), &[root.to_string()], None)
        .await?;
    assert!(!service.gate.complete_retirement(root));
    let config = core_test_support::load_default_config_for_test(&home).await;
    let prepared = crate::service::PreparedTeamHost::load(&config, "installation".into()).await?;
    let restored = crate::TeamHostService::new(prepared, Weak::new());
    assert_eq!(restored.gate.root(root), Some(root));
    assert!(restored.gate.is_retiring(root));
    assert!(restored.gate.is_manually_held(root));
    restored
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 2,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    restored.advance_retirements().await?;
    assert_eq!(
        restored.retirement_states().await?[0].state,
        TeamMemberRetirementState::Retiring
    );
    restored.gate.resume_for_user(root);
    restored
        .store
        .get()
        .unwrap()
        .delete("manual_hold", &root.to_string())
        .await?;
    restored.advance_retirements().await?;
    assert_eq!(
        restored.retirement_states().await?[0].state,
        TeamMemberRetirementState::Removed
    );
    Ok(())
}

#[tokio::test]
async fn self_removal_queues_once_and_returns_before_any_turn_is_drained() -> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let (root, intent) = enroll(&service).await?;
    let work = service.gate.try_enter(root).unwrap();
    let action = TeamAgentActionParams::Member {
        request: TeamMemberActionParams::Remove {
            address: intent.address.clone().into(),
        },
    };
    let response = service
        .agent_action(root, action.clone(), Some("call-one".into()))
        .await?;
    let repeated = service
        .agent_action(root, action, Some("call-one".into()))
        .await?;
    assert_eq!(response.result, repeated.result);
    let operation: TeamAdministrationOperation = serde_json::from_value(response.result)?;
    assert_eq!(operation.state, TeamAdministrationState::Queued);
    assert_eq!(operation.actor, intent.address);
    assert_eq!(
        service
            .store
            .get()
            .unwrap()
            .outbox(0, 100)
            .await?
            .iter()
            .filter(|entry| entry.kind == "administration")
            .count(),
        1
    );
    assert!(!service.gate.is_retiring(root));
    drop(work);
    Ok(())
}

#[tokio::test]
async fn unresolved_external_jobs_are_reported_without_cancellation_or_detachment()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let (root, intent) = enroll(&service).await?;
    let job = crate::jobs::StoredJob {
        job: TeamTrackedJob {
            id: "external-job-1".into(),
            thread_id: root.to_string(),
            server: "jobs".into(),
            tool: "launch".into(),
            handle: Some("handle".into()),
            status: TeamJobStatus::Running,
            detail: None,
        },
        adapter: None,
        launch_revision: Some(1),
        requested_revision: None,
        attempted_revision: None,
        registration_fingerprint: None,
    };
    service
        .store
        .get()
        .unwrap()
        .put("job", &job.job.id, &job)
        .await?;
    service.apply_retirement(&intent).await?;
    service.advance_retirements().await?;
    let state = service.retirement_states().await?.remove(0);
    assert_eq!(state.state, TeamMemberRetirementState::Retiring);
    assert!(
        state
            .waiting_reason
            .as_deref()
            .unwrap()
            .contains("external-job-1 (Running)")
    );
    assert_eq!(service.jobs(&root.to_string()).await?, vec![job.job]);
    assert!(
        service
            .store
            .get()
            .unwrap()
            .enrollment(&root.to_string())
            .await?
            .is_some()
    );
    let error = service
        .leave(TeamLeaveParams {
            thread_id: root.to_string(),
            coordinator_id: "coordinator".into(),
            operation_id: None,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retiring"));
    assert_eq!(service.gate.root(root), Some(root));
    Ok(())
}
