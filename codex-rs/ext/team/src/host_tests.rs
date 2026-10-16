use std::collections::BTreeMap;
use std::sync::Weak;

use codex_app_server_protocol::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnModelRequestInput;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::DeliveryAttempt;
use crate::delivery_evidence::runtime_attempt;
use crate::service::PreparedTeamHost;
use crate::service::TeamHostService;

async fn host() -> anyhow::Result<(tempfile::TempDir, std::sync::Arc<TeamHostService>)> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let prepared = PreparedTeamHost::load(&config, "installation".into()).await?;
    let service = TeamHostService::new(prepared, Weak::new());
    service.enable().await?;
    Ok((home, service))
}

fn delivery(thread_id: &str) -> TeamDelivery {
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: thread_id.into(),
    };
    TeamDelivery {
        id: "delivery".into(),
        thread_id: thread_id.into(),
        mode: TeamDeliveryMode::SafeBoundary,
        wake: true,
        message: TeamMessage {
            id: "message".into(),
            sender: address.clone(),
            recipient: Some(address),
            kind: TeamMessageKind::Request,
            body: "pending peer input".into(),
            reply_to: None,
            topic: None,
            created_at: 1,
        },
    }
}

#[tokio::test]
async fn native_and_mcp_reply_actions_keep_delivery_mode_in_the_durable_outbox()
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
                address,
                alias: "reviewer".into(),
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

    for board in [false, true] {
        let mut original = delivery(&root.to_string());
        original.id = uuid::Uuid::new_v4().to_string();
        original.message.id = uuid::Uuid::new_v4().to_string();
        if board {
            original.message.kind = TeamMessageKind::Board;
            original.message.recipient = None;
            original.message.topic = Some("review".into());
        }
        store.receive(&original).await?;
        for mode in [TeamDeliveryMode::NextTurn, TeamDeliveryMode::SafeBoundary] {
            let mut action = serde_json::json!({"type":"reply", "messageId":original.message.id, "body":"Reviewed"});
            if mode == TeamDeliveryMode::SafeBoundary {
                action["delivery"] = serde_json::json!("safeBoundary");
            }
            let mut serialized = action.clone();
            serialized["delivery"] = serde_json::to_value(mode)?;
            let action: TeamAgentAction = serde_json::from_value(action)?;
            assert_eq!(serde_json::to_value(&action)?, serialized);
            let operation = uuid::Uuid::new_v4().to_string();
            let legacy_result = if !board && mode == TeamDeliveryMode::NextTurn {
                // Model the committed receipt/outbox from the previous wire
                // contract, then retry it through the current reply handler.
                let digest = format!(
                    r#"{{"type":"reply","messageId":"{}","body":"Reviewed"}}"#,
                    original.message.id
                );
                let mut previous = original.message.clone();
                previous.id = uuid::Uuid::new_v4().to_string();
                previous.kind = TeamMessageKind::Reply;
                previous.body = "Reviewed".into();
                previous.reply_to = Some(original.message.id.clone());
                let result = serde_json::json!({"id":previous.id, "state":"queued"});
                store
                    .enqueue_agent_operation(
                        &format!("{root}:{operation}"),
                        &digest,
                        &previous.id,
                        "message",
                        &serde_json::json!({"message":previous, "delivery":"nextTurn"}),
                        &result,
                    )
                    .await?;
                Some(result)
            } else {
                None
            };
            let response = service
                .agent_action(root, action.clone(), Some(operation.clone()))
                .await?;
            if let Some(previous) = legacy_result {
                assert_eq!(response.result, previous);
            }
            assert_eq!(
                service
                    .agent_action(root, action, Some(operation))
                    .await?
                    .result,
                response.result
            );
            let entries = store
                .outbox(0, 100)
                .await?
                .into_iter()
                .filter(|entry| response.result["id"].as_str() == Some(entry.id.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].payload["delivery"], serde_json::to_value(mode)?);
            let reply: TeamMessage = serde_json::from_value(entries[0].payload["message"].clone())?;
            assert_eq!(reply.reply_to.as_ref(), Some(&original.message.id));
            assert_eq!(reply.topic, original.message.topic);
            assert_eq!(
                reply.kind,
                if board {
                    TeamMessageKind::Board
                } else {
                    TeamMessageKind::Reply
                }
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn same_turn_id_completion_requires_participation_of_the_completing_runtime()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let store = service.store.get().unwrap();
    let thread_id = ThreadId::new().to_string();
    let delivery = delivery(&thread_id);
    let initial = store.receive(&delivery).await?;
    store
        .update_receipt(&TeamReceipt {
            state: TeamReceiptState::Interrupted,
            turn_id: Some("same-turn".into()),
            ..initial
        })
        .await?;
    store
        .put(
            "delivery_attempt",
            &delivery.id,
            &DeliveryAttempt {
                id: "dispatch".into(),
                generation: "history".into(),
                admission_revision: 1,
                turn_id: "same-turn".into(),
                acknowledged: true,
                participation: BTreeMap::from([(
                    "original-runtime".into(),
                    Some(AttemptOutcome::Interrupted),
                )]),
                provider_acknowledgements: BTreeMap::new(),
                terminal_outcomes: BTreeMap::new(),
            },
        )
        .await?;
    let thread = ExtensionData::new(&thread_id);
    let resumed = ExtensionData::new("same-turn");
    let runtime = runtime_attempt(&resumed);
    service
        .journal_model_request(TurnModelRequestInput {
            turn_id: "same-turn",
            input: &[],
            thread_store: &thread,
            turn_store: &resumed,
        })
        .await?;
    service
        .finish_deliveries(&thread_id, "same-turn", &runtime, AttemptOutcome::Completed)
        .await?;
    assert_eq!(
        store.deliveries(&thread_id).await?[0].1.state,
        TeamReceiptState::Interrupted
    );

    // A marker persisted after sampling is not request evidence. Only this
    // exact pre-dispatch callback establishes participation of the new runtime.
    let item = serde_json::from_value(
        serde_json::json!({"type": "message", "id": "msg_team-delivery", "role": "user", "content": [{"type": "input_text", "text": "pending peer input"}]}),
    )?;
    service
        .journal_model_request(TurnModelRequestInput {
            turn_id: "same-turn",
            input: &[item],
            thread_store: &thread,
            turn_store: &resumed,
        })
        .await?;
    service
        .finish_deliveries(&thread_id, "same-turn", &runtime, AttemptOutcome::Completed)
        .await?;
    let completed = store.deliveries(&thread_id).await?[0].1.clone();
    assert_eq!(
        completed,
        TeamReceipt {
            delivery_id: "delivery".into(),
            thread_id,
            state: TeamReceiptState::Completed,
            turn_id: Some("same-turn".into()),
            detail: None
        }
    );
    Ok(())
}

#[tokio::test]
async fn failed_participating_attempt_is_durable_and_does_not_complete() -> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let store = service.store.get().unwrap();
    let delivery = delivery(&ThreadId::new().to_string());
    let initial = store.receive(&delivery).await?;
    store
        .update_receipt(&TeamReceipt {
            state: TeamReceiptState::Accepted,
            turn_id: Some("turn".into()),
            ..initial
        })
        .await?;
    store
        .put(
            "delivery_attempt",
            &delivery.id,
            &DeliveryAttempt {
                id: "dispatch".into(),
                generation: "history".into(),
                admission_revision: 1,
                turn_id: "turn".into(),
                acknowledged: true,
                participation: BTreeMap::from([("runtime".into(), None)]),
                provider_acknowledgements: BTreeMap::new(),
                terminal_outcomes: BTreeMap::new(),
            },
        )
        .await?;
    let outcome = AttemptOutcome::Failed("provider rejected request".into());
    service
        .finish_deliveries(&delivery.thread_id, "turn", "runtime", outcome.clone())
        .await?;
    assert_eq!(
        store.deliveries(&delivery.thread_id).await?[0].1.state,
        TeamReceiptState::NeedsReconciliation
    );
    assert_eq!(
        store
            .get::<DeliveryAttempt>("delivery_attempt", &delivery.id)
            .await?
            .unwrap()
            .participation["runtime"],
        Some(outcome)
    );
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "holds ordinary admission and cleanup locks while proving priority control progress"
)]
async fn stop_and_resume_acknowledge_while_ordinary_work_is_blocked_and_resume_remains_fenced()
-> anyhow::Result<()> {
    let (_home, service) = host().await?;
    let store = service.store.get().unwrap();
    let root = ThreadId::new();
    store
        .enroll(
            &TeamEnrollment {
                address: TeamAddress {
                    machine_id: "machine".into(),
                    server_id: "server".into(),
                    thread_id: root.to_string(),
                },
                alias: "root".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            "credential",
        )
        .await?;
    let _ordinary_work = service.operations.lock().await;
    let local_cleanup = service.job_updates.lock().await;
    let stop = service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 2,
                mode: TeamExecutionMode::Stopped,
            },
        })
        .await?;
    let resume = service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 3,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;
    assert_eq!((stop.reconciled, resume.reconciled), (false, false));
    assert!(service.gate.try_enter(root).is_none());
    assert_eq!(service.gate.snapshot(root).unwrap().revision, 3);

    let mut changes = service.gate.subscribe();
    drop(local_cleanup);
    while service.gate.snapshot(root).unwrap().reconciling {
        changes.changed().await?;
    }
    assert!(service.gate.try_enter(root).is_some());
    Ok(())
}
