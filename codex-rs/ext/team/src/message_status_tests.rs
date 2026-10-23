use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn acknowledged_sender_queue_needs_explicit_acceptance_and_status_is_bound_to_the_agent()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let store = service.store.get().unwrap();
    let root = ThreadId::new();
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: root.to_string(),
    };
    store
        .enroll(
            &TeamEnrollment {
                address: address.clone(),
                alias: "Author".into(),
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
                mode: TeamExecutionMode::Stopped,
            },
        })
        .await?;
    let other = TeamAddress {
        thread_id: ThreadId::new().to_string(),
        ..address.clone()
    };
    let result = service
        .agent_action(
            root,
            TeamAgentActionParams::Send {
                recipient: other.clone().into(),
                body: "Please review".into(),
                kind: TeamMessageKind::Request,
                reply_to: None,
                delivery: TeamDeliveryMode::NextTurn,
            },
            Some("stable-operation".into()),
        )
        .await?;
    let id = result.result["id"].as_str().unwrap().to_string();
    let before = service
        .agent_message_status(&address, TeamMessageStatusParams::default())
        .await?;
    assert_eq!(before.data[0].intake, TeamMessageIntake::QueuedOnSenderHost);
    let pending = store.outbox(0, 100).await?;
    store
        .acknowledge(
            &pending
                .into_iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
        )
        .await?;
    assert_eq!(
        service
            .agent_message_status(&address, TeamMessageStatusParams::default())
            .await?,
        before
    );
    assert!(
        service
            .agent_message_status(&other, TeamMessageStatusParams::default())
            .await?
            .data
            .is_empty()
    );

    let rejected = TeamMessageStatus {
        message_id: id.clone(),
        sender: address.clone(),
        sender_name: "Author".into(),
        created_at: Some(1),
        intake: TeamMessageIntake::Rejected,
        recipients: Vec::new(),
        related_replies: Vec::new(),
        detail: Some("recipient is not enrolled".into()),
        freshness: TeamObservationFreshness::Current,
    };
    let sync = TeamSyncParams {
        administration: Vec::new(),
        retirements: Vec::new(),
        board_purges: Vec::new(),
        coordinator_id: "coordinator".into(),
        directory: Default::default(),
        board: Vec::new(),
        board_state: Default::default(),
        cancellation_adapters: Vec::new(),
        message_status: TeamMessageStatusSnapshot {
            revision: 2,
            data: vec![rejected],
        },
    };
    service.sync(sync.clone()).await?;
    let after = service
        .agent_message_status(
            &address,
            TeamMessageStatusParams {
                message_id: Some(id),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(after.data[0].intake, TeamMessageIntake::Rejected);
    assert_eq!(
        after.data[0].freshness,
        TeamObservationFreshness::LastSynchronized
    );
    let mut stale = sync;
    stale.message_status.revision = 1;
    assert!(service.sync(stale).await.is_err());
    assert!(store.outbox(0, 100).await?.is_empty());
    assert!(store.deliveries(&root.to_string()).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn agent_cannot_explain_operator_control_as_an_available_grant() -> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let sender = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: ThreadId::new().to_string(),
    };
    let explanation = service
        .agent_action_explanation(
            &sender,
            TeamExplainActionParams {
                action: TeamExplainableAction::Resume,
                target: None,
            },
        )
        .await?;
    assert_eq!(explanation.availability, TeamActionAvailability::Blocked);
    assert!(explanation.reason.contains("user/operator"));
    assert!(
        service
            .store
            .get()
            .unwrap()
            .outbox(0, 100)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn synchronized_status_scopes_recipients_and_related_replies_before_pagination()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let address = |thread: &str| TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: thread.into(),
    };
    let viewer = address("viewer");
    let author = address("author");
    let other = address("other");
    let recipient = |address: TeamAddress| TeamRecipientMessageStatus {
        name: address.thread_id.clone(),
        address,
        progress: TeamMessageProgress::AdmittedToTurn,
        receipt: None,
        freshness: TeamObservationFreshness::Current,
    };
    let original = TeamMessageStatus {
        message_id: "original".into(),
        sender: author.clone(),
        sender_name: "Author".into(),
        created_at: Some(1),
        intake: TeamMessageIntake::AcceptedAtCoordinator,
        recipients: vec![recipient(viewer.clone()), recipient(other.clone())],
        related_replies: vec!["visible-reply".into(), "secret-reply".into()],
        detail: None,
        freshness: TeamObservationFreshness::Current,
    };
    let visible_reply = TeamMessageStatus {
        message_id: "visible-reply".into(),
        sender: viewer.clone(),
        sender_name: "Viewer".into(),
        recipients: vec![recipient(author)],
        related_replies: Vec::new(),
        ..original.clone()
    };
    let secret_reply = TeamMessageStatus {
        message_id: "secret-reply".into(),
        sender: other.clone(),
        sender_name: "Other".into(),
        recipients: vec![recipient(other)],
        related_replies: Vec::new(),
        ..original.clone()
    };
    service
        .store
        .get()
        .unwrap()
        .put(
            "message_status_cache",
            "current",
            &TeamMessageStatusSnapshot {
                revision: 1,
                data: vec![original.clone(), visible_reply, secret_reply],
            },
        )
        .await?;

    let first = service
        .agent_message_status(
            &viewer,
            TeamMessageStatusParams {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    let mut expected = original;
    expected.recipients.truncate(1);
    expected.recipients[0].freshness = TeamObservationFreshness::LastSynchronized;
    expected.related_replies = vec!["visible-reply".into()];
    expected.freshness = TeamObservationFreshness::LastSynchronized;
    assert_eq!(
        first,
        TeamMessageStatusResponse {
            data: vec![expected],
            next_cursor: Some("original".into())
        }
    );
    let next = service
        .agent_message_status(
            &viewer,
            TeamMessageStatusParams {
                cursor: first.next_cursor,
                limit: Some(1),
                message_id: None,
            },
        )
        .await?;
    assert_eq!(
        next.data
            .iter()
            .map(|status| status.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["visible-reply"]
    );
    assert_eq!(next.next_cursor, None);
    assert!(
        service
            .agent_message_status(
                &viewer,
                TeamMessageStatusParams {
                    message_id: Some("secret-reply".into()),
                    ..Default::default()
                }
            )
            .await?
            .data
            .is_empty()
    );
    assert!(
        service
            .store
            .get()
            .unwrap()
            .outbox(0, 100)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn queued_board_intent_does_not_invent_a_creation_timestamp() -> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let sender = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: ThreadId::new().to_string(),
    };
    service
        .store
        .get()
        .unwrap()
        .enqueue(
            "post",
            "board",
            &serde_json::json!({
                "sender": sender,
                "request": {"type":"post", "topic":"review", "body":"Review notes", "mentions":[]}
            }),
        )
        .await?;
    let status = service
        .agent_message_status(&sender, TeamMessageStatusParams::default())
        .await?;
    assert_eq!(status.data.len(), 1);
    assert_eq!(status.data[0].created_at, None);
    assert_eq!(
        serde_json::to_value(status.data[0].clone())?["createdAt"],
        serde_json::Value::Null
    );
    Ok(())
}
