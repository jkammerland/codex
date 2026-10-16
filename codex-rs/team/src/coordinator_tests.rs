use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;
use uuid::Uuid;

use crate::config::CoordinatorConfig;
use crate::config::Manifest;
use crate::coordinator::Coordinator;
use crate::model::*;

pub(crate) fn manifest(directory: &std::path::Path) -> Manifest {
    Manifest {
        version: 1,
        coordinator: CoordinatorConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            url: "http://127.0.0.1:9000".into(),
            token_file: directory.join("token"),
            state_dir: directory.join("state"),
        },
        machines: Vec::new(),
        gitea: None,
    }
}

pub(crate) async fn agent(coordinator: &Coordinator, name: &str) -> Result<TeamAddress> {
    let address = TeamAddress {
        machine_id: "worker".into(),
        server_id: "account".into(),
        thread_id: Uuid::new_v4().to_string(),
    };
    coordinator
        .store
        .put(
            "agent",
            &address_key(&address),
            &Agent {
                enrollment: TeamEnrollment {
                    address: address.clone(),
                    alias: name.into(),
                    manually_held: false,
                    continuation_turn_id: None,
                },
                requested_selection: None,
                runtime: None,
                status: "idle".into(),
                current_model: None,
                current_effort: None,
                next_model: Some("chosen-model".into()),
                next_effort: Some("medium".into()),
                project: None,
            },
        )
        .await?;
    Ok(address)
}

#[tokio::test]
async fn control_history_survives_restart_and_coordinator_has_single_owner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    assert!(Coordinator::open(manifest.clone()).await.is_err());
    let initial = coordinator.current_control().await?;
    for mode in [
        TeamExecutionMode::Paused,
        TeamExecutionMode::Stopped,
        TeamExecutionMode::Running,
    ] {
        coordinator.execute(TeamRequest::Control { mode }).await?;
    }
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    let history = restored
        .store
        .list::<TeamControlState>("control_history", "", 100)
        .await?;
    assert_eq!(
        history
            .into_iter()
            .map(|(_, control)| (control.revision, control.mode))
            .collect::<Vec<_>>(),
        vec![
            (0, TeamExecutionMode::Running),
            (1, TeamExecutionMode::Paused),
            (2, TeamExecutionMode::Stopped),
            (3, TeamExecutionMode::Running),
        ]
    );
    assert_eq!(
        restored.current_control().await?,
        TeamControlState {
            revision: 3,
            ..initial
        }
    );
    Ok(())
}

#[tokio::test]
async fn paused_board_defers_publication_and_freezes_only_explicit_subscribers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let subscriber = agent(&coordinator, "reviewer").await?;
    let later = agent(&coordinator, "builder").await?;
    coordinator
        .subscribe(subscriber.clone(), "review".into(), true)
        .await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Paused,
        })
        .await?;
    let message = coordinator
        .operator_message(
            None,
            "Please review".into(),
            TeamMessageKind::Board,
            None,
            Some("review".into()),
        )
        .await?;
    let queued = coordinator
        .ingest(message.clone(), TeamDeliveryMode::NextTurn)
        .await?;
    coordinator.publish().await?;
    let pending = coordinator
        .store
        .get::<MessageRecord>("message", &message.id)
        .await?
        .unwrap();
    assert!(!pending.published);
    assert_eq!(pending.targets, None);
    assert_eq!(
        coordinator
            .ingest(message.clone(), TeamDeliveryMode::NextTurn)
            .await?
            .message,
        queued.message
    );

    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        })
        .await?;
    coordinator.publish().await?;
    coordinator.subscribe(later, "review".into(), true).await?;
    coordinator.publish().await?;
    let published = coordinator
        .store
        .get::<MessageRecord>("message", &message.id)
        .await?
        .unwrap();
    assert!(published.published);
    assert_eq!(published.targets, Some(vec![subscriber]));
    let mut conflicting = message;
    conflicting.body = "different body".into();
    assert!(
        coordinator
            .ingest(conflicting, TeamDeliveryMode::NextTurn)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn receipts_are_bound_to_the_target_host_and_survive_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let recipient = agent(&coordinator, "reviewer").await?;
    let message = coordinator
        .operator_message(
            Some(recipient.clone()),
            "review".into(),
            TeamMessageKind::Request,
            None,
            None,
        )
        .await?;
    coordinator
        .ingest(message.clone(), TeamDeliveryMode::NextTurn)
        .await?;
    coordinator.publish().await?;
    let receipt = TeamReceipt {
        delivery_id: format!("{}:{}", message.id, address_key(&recipient)),
        thread_id: recipient.thread_id,
        state: TeamReceiptState::Accepted,
        turn_id: Some("original-turn".into()),
        detail: None,
    };
    assert!(
        coordinator
            .receipt("different-host", "account", receipt.clone())
            .await
            .is_err()
    );
    coordinator
        .receipt("worker", "account", receipt.clone())
        .await?;
    coordinator
        .receipt("worker", "account", receipt.clone())
        .await?;
    let stored = coordinator
        .store
        .get::<MessageRecord>("message", &message.id)
        .await?
        .unwrap();
    assert_eq!(
        stored.receipts.into_values().collect::<Vec<_>>(),
        vec![receipt]
    );
    Ok(())
}

#[test]
fn plaintext_urls_allow_loopback_ipv4_and_ipv6_but_reject_remote_hosts() {
    for value in [
        "http://127.0.0.1:9000",
        "http://[::1]:9000",
        "http://localhost:9000",
        "https://coordinator.example",
    ] {
        assert!(
            crate::config::validate_url(value, &["http", "https"]).is_ok(),
            "{value}"
        );
    }
    for value in [
        "http://coordinator.example",
        "https://user:secret@coordinator.example",
        "https://coordinator.example?token=secret",
    ] {
        assert!(
            crate::config::validate_url(value, &["http", "https"]).is_err(),
            "{value}"
        );
    }
}

#[tokio::test]
async fn replayed_outbox_subscription_cannot_overwrite_a_newer_unsubscribe() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let address = agent(&coordinator, "reviewer").await?;
    let machine = crate::Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: Vec::new(),
    };
    let server = crate::Server {
        id: "account".into(),
        label: "Account".into(),
        endpoint: crate::config::Endpoint::Local {
            socket: directory.path().join("socket"),
        },
        cancellation_adapters: Vec::new(),
    };
    coordinator
        .store
        .put("installation", "worker/account", &"installation-one")
        .await?;
    let entry = TeamOutboxEntry {
        sequence: 1,
        id: Uuid::new_v4().to_string(),
        kind: "subscription".into(),
        payload: serde_json::json!({"sender":address,"topic":"review","enabled":true}),
    };
    coordinator.accept_outbox(&machine, &server, &entry).await?;
    coordinator
        .subscribe(address.clone(), "review".into(), false)
        .await?;
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    restored.accept_outbox(&machine, &server, &entry).await?;
    let subscriptions = restored
        .store
        .list::<crate::routing::Subscription>("subscription", "", 100)
        .await?;
    assert_eq!(subscriptions.len(), 1);
    assert!(!subscriptions[0].1.enabled);
    let conflicting = TeamOutboxEntry {
        payload: serde_json::json!({"sender":address,"topic":"other","enabled":true}),
        ..entry
    };
    assert!(
        restored
            .accept_outbox(&machine, &server, &conflicting)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn retried_operation_keeps_its_effect_and_result_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let operation = TeamOperation::new(TeamRequest::Post {
        topic: "review".into(),
        body: "Please inspect".into(),
    });
    let response = serde_json::to_value(coordinator.execute_operation(operation.clone()).await?)?;
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    assert_eq!(
        serde_json::to_value(restored.execute_operation(operation.clone()).await?)?,
        response
    );
    assert_eq!(restored.snapshot().await?.messages.len(), 1);
    let conflict = TeamOperation {
        request: TeamRequest::Post {
            topic: "review".into(),
            body: "Changed".into(),
        },
        ..operation
    };
    assert!(restored.execute_operation(conflict).await.is_err());

    let stop = TeamOperation::new(TeamRequest::Control {
        mode: TeamExecutionMode::Stopped,
    });
    restored.execute_operation(stop.clone()).await?;
    restored
        .execute_operation(TeamOperation::new(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        }))
        .await?;
    restored.execute_operation(stop).await?;
    assert_eq!(
        restored.current_control().await?.mode,
        TeamExecutionMode::Running
    );
    assert_eq!(restored.current_control().await?.revision, 2);
    Ok(())
}

#[tokio::test]
async fn replies_preserve_the_selected_boundary_for_direct_board_and_gitea_conversations()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let recipient = agent(&coordinator, "reviewer").await?;
    let mut replies = Vec::new();

    for conversation in ["direct", "board", "gitea"] {
        let board = conversation == "board";
        let mut original = coordinator
            .operator_message(
                (!board).then_some(recipient.clone()),
                "Please review".into(),
                if board {
                    TeamMessageKind::Board
                } else {
                    TeamMessageKind::Request
                },
                None,
                board.then_some("review".into()),
            )
            .await?;
        if conversation == "direct" {
            original.sender = recipient.clone();
        }
        if conversation == "gitea" {
            original.sender = TeamAddress {
                machine_id: "gitea".into(),
                server_id: "example/project".into(),
                thread_id: "12".into(),
            };
            coordinator
                .store
                .put(
                    "gitea_origin",
                    &original.id,
                    &crate::gitea::Origin {
                        integration: None,
                        repository: "example/project".into(),
                        number: 12,
                        url: "https://example.invalid/project/issues/12".into(),
                    },
                )
                .await?;
        }
        let original = coordinator
            .ingest(original, TeamDeliveryMode::NextTurn)
            .await?
            .message;
        for delivery in [TeamDeliveryMode::NextTurn, TeamDeliveryMode::SafeBoundary] {
            // Omitted delivery is the existing client contract. Explicit
            // safe-boundary mode must survive both routing and durable retry.
            let mut request =
                serde_json::json!({"type":"reply", "messageId":original.id, "body":"Reviewed"});
            if delivery == TeamDeliveryMode::SafeBoundary {
                request["delivery"] = serde_json::json!("safeBoundary");
            }
            let operation = TeamOperation::new(serde_json::from_value(request.clone())?);
            assert_eq!(serde_json::to_value(&operation.request)?, request);
            let TeamResponse::Message { message } =
                coordinator.execute_operation(operation.clone()).await?
            else {
                anyhow::bail!("reply did not return its durable message");
            };
            assert_eq!(message.delivery, delivery);
            assert_eq!(message.message.reply_to.as_ref(), Some(&original.id));
            assert_eq!(message.message.topic, original.topic);
            assert_eq!(
                message.message.kind,
                if board {
                    TeamMessageKind::Board
                } else {
                    TeamMessageKind::Reply
                }
            );
            assert_eq!(
                message.message.recipient,
                (!board).then_some(original.sender.clone())
            );
            let response = serde_json::to_value(TeamResponse::Message { message })?;
            assert_eq!(
                serde_json::to_value(coordinator.execute_operation(operation.clone()).await?)?,
                response
            );
            replies.push((operation, delivery, response));
        }
    }
    drop(coordinator);

    let recovered = Coordinator::open(manifest).await?;
    for (operation, delivery, response) in replies {
        assert_eq!(
            serde_json::to_value(recovered.execute_operation(operation.clone()).await?)?,
            response
        );
        assert_eq!(
            recovered
                .store
                .get::<MessageRecord>("message", &operation.operation_id)
                .await?
                .unwrap()
                .delivery,
            delivery
        );
    }
    Ok(())
}

#[test]
fn machine_names_cannot_impersonate_external_inboxes_and_root_ids_are_canonical() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for name in ["operator", "gitea", "OPERATOR"] {
        let mut manifest = manifest(directory.path());
        manifest.machines.push(crate::Machine {
            id: name.into(),
            label: name.into(),
            servers: Vec::new(),
        });
        assert!(manifest.validate().is_err());
    }
    let id = Uuid::new_v4();
    let address = canonical_address(TeamAddress {
        machine_id: "worker".into(),
        server_id: "account".into(),
        thread_id: id.simple().to_string().to_uppercase(),
    })?;
    assert_eq!(address.thread_id, id.to_string());
    Ok(())
}

#[tokio::test]
async fn leaving_forgets_pending_model_intent_even_when_directory_row_is_already_gone() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(manifest(directory.path())).await?;
    let address = agent(&coordinator, "reviewer").await?;
    let key = address_key(&address);
    let mut rejoined = coordinator
        .store
        .get::<Agent>("agent", &key)
        .await?
        .unwrap();
    coordinator.store.put("model_request", &key, &serde_json::json!({
        "selection":{"revision":9,"model":{"type":"value","value":"old-choice"},"effort":{"type":"keep"}},
        "model":"old-choice", "effort":null
    })).await?;
    coordinator.store.delete("agent", &key).await?;

    coordinator
        .forget_agents(&std::collections::HashSet::from([key.clone()]))
        .await?;
    rejoined.next_model = Some("chosen-while-outside-team".into());
    coordinator.store.put("agent", &key, &rejoined).await?;
    assert_eq!(
        coordinator
            .store
            .get::<serde_json::Value>("model_request", &key)
            .await?,
        None
    );
    assert_eq!(
        coordinator.store.get::<Agent>("agent", &key).await?,
        Some(rejoined)
    );
    Ok(())
}
