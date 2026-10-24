use super::*;
use clap::Parser;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn disposition_outbox_is_durable_attributed_and_keeps_the_original_receipt() -> Result<()> {
    let home = tempfile::tempdir()?;
    let manifest = crate::tests::manifest(home.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let address = crate::tests::agent(&coordinator, "Reviewer").await?;
    let TeamResponse::Message { mut message } = coordinator
        .execute(TeamRequest::Send {
            recipient: address.clone(),
            body: "Inspect this".into(),
            kind: TeamMessageKind::Request,
            reply_to: None,
            delivery: TeamDeliveryMode::NextTurn,
        })
        .await?
    else {
        panic!("message response")
    };
    let message_id = message.message.id.clone();
    let delivery_id = format!("{message_id}:{}", address_key(&address));
    let receipt = TeamReceipt {
        delivery_id: delivery_id.clone(),
        thread_id: address.thread_id.clone(),
        state: TeamReceiptState::NeedsReconciliation,
        turn_id: Some("failed-turn".into()),
        detail: Some("Authentication failed".into()),
    };
    message.targets = Some(vec![address.clone()]);
    message
        .receipts
        .insert(delivery_id.clone(), receipt.clone());
    coordinator
        .store
        .put("message", &message_id, &message)
        .await?;
    let machine = crate::Machine {
        id: address.machine_id.clone(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        id: address.server_id.clone(),
        label: "Primary".into(),
        endpoint: crate::Endpoint::Local {
            socket: home.path().join("socket"),
        },
        environment_capabilities: Default::default(),
        cancellation_adapters: Vec::new(),
    };
    coordinator
        .store
        .put(
            "installation",
            &server_key(&machine.id, &server.id),
            &"installation",
        )
        .await?;
    let abandonment = TeamDeliveryAbandonment {
        operation_id: uuid::Uuid::now_v7().to_string(),
        delivery_id: delivery_id.clone(),
        thread_id: address.thread_id.clone(),
        reason: "Inspected failed attempt; stop further processing".into(),
    };
    let entry = TeamOutboxEntry {
        sequence: 1,
        id: format!("abandonment:{}", abandonment.operation_id),
        kind: "delivery_abandonment".into(),
        payload: serde_json::to_value(&abandonment)?,
    };
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    let status = coordinator
        .message_status(TeamMessageStatusParams {
            message_id: Some(message_id.clone()),
            ..Default::default()
        })
        .await?;
    assert_eq!(status.data[0].recipients[0].receipt, Some(receipt.clone()));
    assert_eq!(
        status.data[0].recipients[0].progress,
        TeamMessageProgress::ProcessingUncertain
    );
    assert_eq!(
        status.data[0].recipients[0].abandonment,
        Some(abandonment.clone())
    );
    assert!(coordinator.snapshot().await?.rejected_outbox.is_empty());

    // Another host cannot settle this delivery, even when it submits a valid disposition.
    assert!(
        coordinator
            .prepare_delivery_abandonment("other-host", &server.id, &abandonment)
            .await
            .is_err()
    );
    drop(coordinator);
    let restored = Coordinator::open(manifest).await?;
    let record = restored
        .store
        .get::<MessageRecord>("message", &message_id)
        .await?
        .unwrap();
    assert_eq!(record.receipts.get(&delivery_id), Some(&receipt));
    assert_eq!(record.abandonments.get(&delivery_id), Some(&abandonment));
    Ok(())
}

#[test]
fn abandon_delivery_requires_an_address_message_and_operator_reason() {
    let address = format!("worker/primary/{}", uuid::Uuid::new_v4());
    let parsed = crate::TeamCommand::try_parse_from([
        "agents",
        "abandon-delivery",
        &address,
        "message",
        "--reason",
        "Inspected failure",
    ])
    .unwrap();
    assert!(
        matches!(parsed.command, crate::cli::TeamAction::AbandonDelivery { message_id, reason, .. } if message_id == "message" && reason == "Inspected failure")
    );
    assert!(
        crate::TeamCommand::try_parse_from(["agents", "abandon-delivery", &address, "message"])
            .is_err()
    );
}

#[tokio::test]
async fn disconnected_worker_recovery_reserves_operation_identity_across_restart() -> Result<()> {
    let home = tempfile::tempdir()?;
    let manifest = crate::tests::manifest(home.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let address = crate::tests::agent(&coordinator, "Reviewer").await?;
    let TeamResponse::Message { mut message } = coordinator
        .execute(TeamRequest::Send {
            recipient: address.clone(),
            body: "Inspect this".into(),
            kind: TeamMessageKind::Request,
            reply_to: None,
            delivery: TeamDeliveryMode::NextTurn,
        })
        .await?
    else {
        panic!("message response")
    };
    message.targets = Some(vec![address.clone()]);
    coordinator
        .store
        .put("message", &message.message.id, &message)
        .await?;
    let operation = TeamOperation::new(TeamRequest::AbandonDelivery {
        address,
        message_id: message.message.id.clone(),
        reason: "Inspected failed attempt".into(),
    });
    let control = coordinator.current_control().await?;
    assert!(
        coordinator
            .execute_operation(operation.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("worker is not connected")
    );
    let params = coordinator
        .store
        .get::<TeamDeliveryAbandonParams>("delivery_abandon_request", &operation.operation_id)
        .await?
        .unwrap();
    let conflicting = TeamOperation {
        operation_id: operation.operation_id.clone(),
        request: TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        },
    };
    assert!(
        coordinator
            .execute_operation(conflicting.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting content")
    );
    assert_eq!(coordinator.current_control().await?, control);
    let mut changed = operation.clone();
    let TeamRequest::AbandonDelivery { reason, .. } = &mut changed.request else {
        unreachable!()
    };
    *reason = "Different disposition".into();
    assert!(
        coordinator
            .execute_operation(changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting content")
    );
    let message_count = coordinator.snapshot().await?.messages.len();
    let conflicting_send = TeamOperation {
        operation_id: operation.operation_id.clone(),
        request: TeamRequest::Post {
            topic: "work".into(),
            body: "Conflicting operation".into(),
        },
    };
    assert!(
        coordinator
            .execute_operation(conflicting_send)
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting content")
    );
    assert_eq!(coordinator.snapshot().await?.messages.len(), message_count);
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    assert!(
        restored
            .execute_operation(operation.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("worker is not connected")
    );
    assert_eq!(
        restored
            .store
            .get::<TeamDeliveryAbandonParams>("delivery_abandon_request", &operation.operation_id)
            .await?,
        Some(params)
    );
    assert!(
        restored
            .execute_operation(conflicting)
            .await
            .unwrap_err()
            .to_string()
            .contains("conflicting content")
    );
    assert_eq!(restored.current_control().await?, control);
    Ok(())
}
