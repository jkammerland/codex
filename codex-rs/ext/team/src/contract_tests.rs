use super::*;
use pretty_assertions::assert_eq;

async fn enrolled() -> anyhow::Result<(
    tempfile::TempDir,
    std::sync::Arc<TeamHostService>,
    TeamAddress,
)> {
    let (home, service) = host().await?;
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: ThreadId::new().to_string(),
    };
    service
        .store
        .get()
        .unwrap()
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
                mode: TeamExecutionMode::Paused,
            },
        })
        .await?;
    Ok((home, service, address))
}

fn teammate(address: TeamAddress, alias: &str) -> TeamDirectoryEntry {
    TeamDirectoryEntry {
        address,
        alias: alias.into(),
        manually_held: false,
        continuation_turn_id: None,
        discovery: Default::default(),
        current_model: None,
        current_effort: None,
        next_model: None,
        next_effort: None,
    }
}

#[tokio::test]
async fn native_send_and_credential_retry_freeze_alias_resolution() -> anyhow::Result<()> {
    let (_home, service, author) = enrolled().await?;
    let recipient = TeamAddress {
        thread_id: ThreadId::new().to_string(),
        ..author.clone()
    };
    let store = service.store.get().unwrap();
    store
        .put(
            "directory_cache",
            "current",
            &TeamDirectorySnapshot {
                revision: 1,
                data: vec![teammate(recipient.clone(), "Reviewer")],
            },
        )
        .await?;
    let action: TeamAgentActionParams = serde_json::from_value(
        serde_json::json!({"type":"send","recipient":"Reviewer","body":"Review once"}),
    )?;
    let root = ThreadId::from_string(&author.thread_id)?;
    let first = service
        .agent_action(root, action.clone(), Some("alias-call".into()))
        .await?;
    assert_eq!(
        first.result["route"],
        serde_json::json!({"type":"direct","recipient":recipient})
    );
    assert_eq!(first.result["state"], "queued");
    assert_eq!(first.result["delivery"], "nextTurn");

    // A renamed/reassigned alias must not redirect a lost-response retry.
    let replacement = TeamAddress {
        thread_id: ThreadId::new().to_string(),
        ..author.clone()
    };
    store
        .put(
            "directory_cache",
            "current",
            &TeamDirectorySnapshot {
                revision: 2,
                data: vec![teammate(replacement, "Reviewer")],
            },
        )
        .await?;
    let retried = service
        .agent_request(TeamAgentRequestParams {
            thread_id: author.thread_id,
            credential: "credential".into(),
            operation_id: Some("alias-call".into()),
            action,
        })
        .await?;
    assert_eq!(retried, first);
    let queued = store.outbox(0, 100).await?;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].payload["message"]["recipient"],
        serde_json::to_value(recipient)?
    );
    Ok(())
}

#[tokio::test]
async fn directory_defaults_are_compact_and_details_are_explicit() -> anyhow::Result<()> {
    let (_home, service, author) = enrolled().await?;
    let entries = (0..21)
        .map(|index| {
            teammate(
                TeamAddress {
                    thread_id: format!("thread-{index:02}"),
                    ..author.clone()
                },
                &format!("Reviewer-{index:02}"),
            )
        })
        .collect::<Vec<_>>();
    service
        .store
        .get()
        .unwrap()
        .put(
            "directory_cache",
            "current",
            &TeamDirectorySnapshot {
                revision: 1,
                data: entries,
            },
        )
        .await?;
    let first = service.agent_directory(None, None, None, false).await?;
    assert_eq!(first["data"].as_array().unwrap().len(), 20);
    assert!(first["data"][0].get("currentModel").is_none());
    assert!(first["data"][0].get("availability").is_some());
    let second = service
        .agent_directory(
            Some(first["nextCursor"].as_str().unwrap().into()),
            None,
            None,
            false,
        )
        .await?;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    let detail = service
        .agent_directory(None, None, Some("Reviewer-20".into()), true)
        .await?;
    assert_eq!(detail["data"].as_array().unwrap().len(), 1);
    assert!(detail["data"][0].get("currentModel").is_some());
    Ok(())
}

#[tokio::test]
async fn inbox_pages_follow_arrival_order_not_sender_ids() -> anyhow::Result<()> {
    let (_home, service, author) = enrolled().await?;
    let store = service.store.get().unwrap();
    for (id, message_id) in [("arrival-one", "z-newer"), ("arrival-two", "a-older")] {
        let mut message = delivery(&author.thread_id);
        message.id = id.into();
        message.message.id = message_id.into();
        store.receive(&message).await?;
    }
    let first = service
        .agent_inbox(&author.thread_id, None, Some(1))
        .await?;
    assert_eq!(first["data"][0]["id"], "z-newer");
    assert!(first["data"][0].get("receipt").is_some());
    let next = service
        .agent_inbox(
            &author.thread_id,
            Some(first["nextCursor"].as_str().unwrap().into()),
            Some(1),
        )
        .await?;
    assert_eq!(next["data"][0]["id"], "a-older");
    assert_eq!(next["nextCursor"], serde_json::Value::Null);
    assert_eq!(
        store.deliveries(&author.thread_id).await?[0].1.state,
        TeamReceiptState::Pending
    );
    Ok(())
}

#[tokio::test]
async fn publish_binds_alias_mentions_and_reports_no_implicit_subscription() -> anyhow::Result<()> {
    let (_home, service, author) = enrolled().await?;
    let store = service.store.get().unwrap();
    let target = TeamAddress {
        thread_id: ThreadId::new().to_string(),
        ..author.clone()
    };
    store
        .put(
            "directory_cache",
            "current",
            &TeamDirectorySnapshot {
                revision: 1,
                data: vec![teammate(target.clone(), "Ana")],
            },
        )
        .await?;
    store
        .put("board_cache", "current", &TeamBoardSnapshot::default())
        .await?;
    let action: TeamAgentActionParams = serde_json::from_value(
        serde_json::json!({"type":"publish","topic":"reviews","body":"🧪 @Ana please review","mentions":[{"target":"Ana"}]}),
    )?;
    let result = service
        .agent_request(TeamAgentRequestParams {
            thread_id: author.thread_id,
            credential: "credential".into(),
            operation_id: Some("publish".into()),
            action,
        })
        .await?;
    let queued = store.outbox(0, 100).await?;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].payload["request"]["mentions"],
        serde_json::json!([{"identity":{"type":"agent","address":target},"display":"@Ana","start":5,"end":9}])
    );
    assert_eq!(result.result["route"]["type"], "board");
    assert_eq!(result.result["subscriptionMode"], "off");
    assert_eq!(
        result.result["notificationPolicy"],
        "mentionsOrWakeSubscriptions"
    );
    Ok(())
}

#[tokio::test]
async fn passive_notes_reject_active_boundary_before_queueing() -> anyhow::Result<()> {
    let (_home, service, author) = enrolled().await?;
    let root = ThreadId::from_string(&author.thread_id)?;
    let action = TeamAgentActionParams::Send {
        recipient: author.into(),
        body: "Progress".into(),
        kind: TeamMessageKind::Note,
        reply_to: None,
        delivery: TeamDeliveryMode::SafeBoundary,
    };
    let error = service.agent_action(root, action, None).await.unwrap_err();
    assert!(error.to_string().contains("passive notes require nextTurn"));
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
