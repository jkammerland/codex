use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn completed_turn_is_evidence_without_an_expected_reply_and_status_pages_are_read_only()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let recipient = crate::tests::agent(&coordinator, "Reviewer").await?;
    let response = coordinator
        .execute(TeamRequest::Send {
            recipient: recipient.clone(),
            body: "Review when convenient".into(),
            kind: TeamMessageKind::Request,
            reply_to: None,
            delivery: TeamDeliveryMode::NextTurn,
        })
        .await?;
    let TeamResponse::Message { mut message } = response else {
        panic!("message response")
    };
    let id = message.message.id.clone();
    let delivery_id = format!("{id}:{}", address_key(&recipient));
    message.targets = Some(vec![recipient.clone()]);
    message.receipts.insert(
        delivery_id.clone(),
        TeamReceipt {
            delivery_id,
            thread_id: recipient.thread_id.clone(),
            state: TeamReceiptState::Completed,
            turn_id: Some("turn".into()),
            detail: None,
        },
    );
    coordinator.store.put("message", &id, &message).await?;
    let first = coordinator
        .message_status(TeamMessageStatusParams::default())
        .await?;
    assert_eq!(
        first.data[0].recipients[0].progress,
        TeamMessageProgress::TurnEnded
    );
    assert_eq!(first.data[0].related_replies, Vec::<String>::new());
    assert_eq!(first.data[0].recipients[0].name, "Reviewer");
    assert_eq!(
        coordinator
            .message_status(TeamMessageStatusParams {
                cursor: Some(id.clone()),
                ..Default::default()
            })
            .await?
            .data,
        Vec::new()
    );
    assert_eq!(
        coordinator
            .message_status(TeamMessageStatusParams::default())
            .await?,
        first
    );
    assert_eq!(coordinator.snapshot().await?.messages.len(), 1);

    let reply = coordinator
        .execute(TeamRequest::Reply {
            message_id: id,
            body: "Optional follow-up".into(),
            delivery: TeamDeliveryMode::NextTurn,
        })
        .await?;
    let TeamResponse::Message { message: reply } = reply else {
        panic!("reply response")
    };
    let after = coordinator
        .message_status(TeamMessageStatusParams {
            message_id: Some(message.message.id),
            ..Default::default()
        })
        .await?;
    assert_eq!(after.data[0].recipients, first.data[0].recipients);
    assert_eq!(after.data[0].related_replies, vec![reply.message.id]);
    Ok(())
}

#[tokio::test]
async fn rejected_intake_projects_rejection_and_snapshot_revision_changes_only_with_evidence()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let sender = crate::tests::agent(&coordinator, "Author").await?;
    let machine = crate::Machine {
        id: sender.machine_id.clone(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        id: sender.server_id.clone(),
        label: "Account".into(),
        environment_capabilities: Default::default(),
        endpoint: crate::Endpoint::Local {
            socket: directory.path().join("socket"),
        },
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
    let id = uuid::Uuid::new_v4().to_string();
    let entry = TeamOutboxEntry {
        sequence: 1,
        id: id.clone(),
        kind: "message".into(),
        payload: serde_json::json!({"message": TeamMessage {
        id: id.clone(), sender: sender.clone(), recipient: Some(TeamAddress {thread_id: uuid::Uuid::new_v4().to_string(), ..sender.clone()}), kind: TeamMessageKind::Request, body: "Invalid recipient".into(), reply_to: None, topic: None, created_at: 1,
    }, "delivery": TeamDeliveryMode::NextTurn}),
    };
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    let first = coordinator.message_status_snapshot().await?;
    assert_eq!(first.data.len(), 1);
    assert_eq!(first.data[0].intake, TeamMessageIntake::Rejected);
    assert!(
        first.data[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("recipient is not enrolled")
    );
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    assert_eq!(coordinator.message_status_snapshot().await?, first);
    Ok(())
}

#[tokio::test]
async fn queueing_is_available_while_processing_is_held_and_leave_requires_stop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let recipient = crate::tests::agent(&coordinator, "Reviewer").await?;
    let mut snapshot = coordinator.snapshot().await?;
    snapshot.control.mode = TeamExecutionMode::Paused;
    snapshot.agents[0].discovery.availability.readiness = TeamReadiness::Held;
    snapshot.agents[0].discovery.availability.reasons = vec!["Team is paused".into()];
    snapshot.servers.push(ServerStatus {
        machine_id: recipient.machine_id.clone(),
        machine_label: "Worker".into(),
        server_id: recipient.server_id.clone(),
        server_label: "Account".into(),
        installation_id: None,
        connected: true,
        applied_revision: Some(snapshot.control.revision),
        error: None,
    });
    let explain = |action| {
        crate::explain_action(
            &snapshot,
            TeamExplainActionParams {
                action,
                target: Some(recipient.clone()),
            },
        )
    };
    assert_eq!(
        explain(TeamExplainableAction::QueueMessage).availability,
        TeamActionAvailability::Available
    );
    assert_eq!(
        explain(TeamExplainableAction::ProcessMessages).availability,
        TeamActionAvailability::Blocked
    );
    assert_eq!(
        explain(TeamExplainableAction::Leave).availability,
        TeamActionAvailability::Blocked
    );
    assert!(
        explain(TeamExplainableAction::Resume)
            .reason
            .contains("Manual holds")
    );
    snapshot.servers[0].connected = false;
    assert_eq!(
        crate::explain_action(
            &snapshot,
            TeamExplainActionParams {
                action: TeamExplainableAction::ProcessMessages,
                target: Some(recipient)
            }
        )
        .availability,
        TeamActionAvailability::Unconfirmed
    );
    Ok(())
}
