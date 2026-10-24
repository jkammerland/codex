use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_protocol::*;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInputRequest;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnModelRequestInput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::PreparedTeamHost;
use super::TeamHostService;
use crate::delivery::DeliveryBoundary;
use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::DeliveryAttempt;
use crate::delivery_evidence::runtime_attempt;

fn user_input(text: &str) -> TurnInputRequest {
    TurnInputRequest::user_input(vec![UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }])
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "owns the dispatch lane so background events cannot satisfy delivery assertions"
)]
async fn passive_safe_boundary_note_waits_for_ordinary_turn_while_waking_note_steers()
-> anyhow::Result<()> {
    let (release_first, first_gate) = oneshot::channel();
    let (release_second, second_gate) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: Some(first_gate),
            body: responses::sse_completed("first-turn"),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse_completed("waking-note"),
        }],
        vec![StreamingSseChunk {
            gate: Some(second_gate),
            body: responses::sse_completed("second-turn"),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse_completed("passive-note"),
        }],
    ])
    .await;
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let fixture = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&model)
        .await?;
    let thread_id = fixture.session_configured.thread_id;
    let mut prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    prepared.gate = gate;
    let service = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: thread_id.to_string(),
    };
    service
        .enroll(TeamEnrollParams {
            operation_id: None,
            address: address.clone(),
            alias: "worker".into(),
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 1,
                mode: TeamExecutionMode::Running,
            },
        })
        .await?;

    // Dispatch runs synchronously while the first response cannot finish.
    // No lifecycle contributor may consume the note on a different boundary.
    let _operation = service.operations.lock().await;
    let StartIfIdleSubmission::Started { turn_id: first, .. } = fixture
        .codex
        .start_turn_if_idle(user_input("First ordinary turn"))
        .await?
    else {
        anyhow::bail!("first ordinary turn must start");
    };
    timeout(Duration::from_secs(10), model.wait_for_request_count(1)).await?;
    assert_eq!(
        fixture.codex.team_active_turn_id().await,
        Some(first.clone())
    );

    let store = service.store.get().unwrap();
    let passive = TeamDelivery {
        id: "passive".into(),
        thread_id: thread_id.to_string(),
        mode: TeamDeliveryMode::SafeBoundary,
        wake: false,
        message: TeamMessage {
            id: "passive-message".into(),
            sender: address.clone(),
            recipient: Some(address),
            kind: TeamMessageKind::Note,
            body: "PASSIVE_NOTE_ONLY_ON_ORDINARY_TURN".into(),
            reply_to: None,
            topic: None,
            created_at: 1,
        },
    };
    let pending = store.receive(&passive).await?;
    assert_eq!(pending.state, TeamReceiptState::Pending);
    service
        .dispatch_delivery(thread_id, DeliveryBoundary::IdleOrSafeBoundary)
        .await?;
    assert_eq!(
        store.deliveries(&thread_id.to_string()).await?,
        vec![(passive.clone(), pending.clone())]
    );
    assert!(
        store
            .get::<DeliveryAttempt>("delivery_attempt", &passive.id)
            .await?
            .is_none(),
        "passive dispatch must not attempt thread delivery"
    );

    // The same mode with wake enabled is a positive control for active steering.
    let mut waking = passive.clone();
    waking.id = "waking".into();
    waking.wake = true;
    waking.message.id = "waking-message".into();
    waking.message.body = "WAKING_NOTE_STEERS_CURRENT_TURN".into();
    let mut waking_receipt = store.receive(&waking).await?;
    service
        .dispatch_delivery(thread_id, DeliveryBoundary::IdleOrSafeBoundary)
        .await?;
    waking_receipt.state = TeamReceiptState::Accepted;
    waking_receipt.turn_id = Some(first.clone());
    assert_eq!(
        store.deliveries(&thread_id.to_string()).await?,
        vec![
            (passive.clone(), pending.clone()),
            (waking.clone(), waking_receipt.clone()),
        ]
    );
    assert_eq!(fixture.codex.team_active_turn_id().await, Some(first));
    release_first
        .send(())
        .expect("first response remains gated");
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&fixture.codex).await;
    let requests = model.requests().await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let request: serde_json::Value = serde_json::from_slice(request)?;
        assert!(!request["input"].to_string().contains(&passive.message.body));
    }
    let steered: serde_json::Value = serde_json::from_slice(&requests[1])?;
    assert!(steered["input"].to_string().contains(&waking.message.body));

    // The pending note is still passive while idle, but an ordinary turn's
    // explicit start boundary admits it without requiring wake permission.
    service
        .dispatch_delivery(thread_id, DeliveryBoundary::IdleOrSafeBoundary)
        .await?;
    assert_eq!(fixture.codex.team_active_turn_id().await, None);
    assert_eq!(
        store.deliveries(&thread_id.to_string()).await?,
        vec![
            (passive.clone(), pending.clone()),
            (waking.clone(), waking_receipt.clone()),
        ]
    );
    let StartIfIdleSubmission::Started {
        turn_id: second, ..
    } = fixture
        .codex
        .start_turn_if_idle(user_input("Second ordinary turn"))
        .await?
    else {
        anyhow::bail!("second ordinary turn must start");
    };
    timeout(Duration::from_secs(10), model.wait_for_request_count(3)).await?;
    service
        .dispatch_delivery(thread_id, DeliveryBoundary::TurnStart { turn_id: &second })
        .await?;
    let mut admitted = pending;
    admitted.state = TeamReceiptState::Accepted;
    admitted.turn_id = Some(second);
    assert_eq!(
        store.deliveries(&thread_id.to_string()).await?,
        vec![(passive.clone(), admitted), (waking, waking_receipt)]
    );
    release_second
        .send(())
        .expect("second response remains gated");
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&fixture.codex).await;
    let requests = model.requests().await;
    assert_eq!(requests.len(), 4);
    let delivered: serde_json::Value = serde_json::from_slice(&requests[3])?;
    assert!(
        delivered["input"]
            .to_string()
            .contains(&passive.message.body)
    );

    fixture.codex.shutdown_and_wait().await?;
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn later_turn_outcomes_require_the_exact_participating_runtime() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let prepared = PreparedTeamHost::load(&config, "installation".into()).await?;
    let service = TeamHostService::new(prepared, std::sync::Weak::new());
    service.enable().await?;
    let store = service.store.get().unwrap();
    let thread_id = codex_protocol::ThreadId::new().to_string();
    let thread = ExtensionData::new(&thread_id);
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: thread_id.clone(),
    };

    for (index, initial_state, outcome, expected_state, expected_detail) in [
        (
            0,
            TeamReceiptState::Interrupted,
            AttemptOutcome::Completed,
            TeamReceiptState::Completed,
            None,
        ),
        (
            1,
            TeamReceiptState::Interrupted,
            AttemptOutcome::Interrupted,
            TeamReceiptState::Interrupted,
            None,
        ),
        (
            2,
            TeamReceiptState::Interrupted,
            AttemptOutcome::Failed("provider rejected request".into()),
            TeamReceiptState::NeedsReconciliation,
            Some("Participating turn failed: provider rejected request".into()),
        ),
        (
            3,
            TeamReceiptState::Completed,
            AttemptOutcome::Interrupted,
            TeamReceiptState::Completed,
            None,
        ),
    ] {
        let delivery = TeamDelivery {
            id: format!("cross-turn-{index}"),
            thread_id: thread_id.clone(),
            mode: TeamDeliveryMode::SafeBoundary,
            wake: true,
            message: TeamMessage {
                id: format!("message-{index}"),
                sender: address.clone(),
                recipient: Some(address.clone()),
                kind: TeamMessageKind::Request,
                body: "input retained from an interrupted turn".into(),
                reply_to: None,
                topic: None,
                created_at: 1,
            },
        };
        let initial = TeamReceipt {
            state: initial_state,
            turn_id: Some("original-turn".into()),
            ..store.receive(&delivery).await?
        };
        store.update_receipt(&initial).await?;
        let original_outcome = if initial_state == TeamReceiptState::Completed {
            AttemptOutcome::Completed
        } else {
            AttemptOutcome::Interrupted
        };
        let mut expected_attempt = DeliveryAttempt {
            id: "original-dispatch".into(),
            generation: "original-history".into(),
            admission_revision: 1,
            turn_id: "original-turn".into(),
            acknowledged: true,
            participation: BTreeMap::from([(
                "original-runtime".into(),
                Some(original_outcome.clone()),
            )]),
            provider_acknowledgements: BTreeMap::new(),
            terminal_outcomes: BTreeMap::from([("original-runtime".into(), original_outcome)]),
        };
        store
            .put("delivery_attempt", &delivery.id, &expected_attempt)
            .await?;

        // A later request may include the original marker repeatedly without
        // creating another dispatch or replacing the original runtime's result.
        let later = ExtensionData::new("later-turn");
        let runtime = runtime_attempt(&later);
        let marker = serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": format!("msg_team-{}", delivery.id),
            "role": "user",
            "content": [{"type": "input_text", "text": delivery.message.body}],
        }))?;
        for _ in 0..2 {
            service
                .journal_model_request(TurnModelRequestInput {
                    turn_id: "later-turn",
                    input: std::slice::from_ref(&marker),
                    thread_store: &thread,
                    turn_store: &later,
                })
                .await?;
        }
        expected_attempt.participation.insert(runtime.clone(), None);

        // Sharing the later turn ID does not grant an unrelated runtime
        // ownership of another request's delivery participation.
        service
            .finish_deliveries(
                &thread_id,
                "later-turn",
                "unrelated-runtime",
                AttemptOutcome::Completed,
            )
            .await?;
        assert_eq!(store.deliveries(&thread_id).await?[index].1, initial);
        assert_eq!(
            store
                .get::<serde_json::Value>("delivery_attempt", &delivery.id)
                .await?,
            Some(serde_json::to_value(&expected_attempt)?)
        );

        service
            .finish_deliveries(&thread_id, "later-turn", &runtime, outcome.clone())
            .await?;
        expected_attempt
            .participation
            .insert(runtime.clone(), Some(outcome.clone()));
        expected_attempt.terminal_outcomes.insert(runtime, outcome);

        // Repeated evidence after completion must not erase a known outcome.
        service
            .journal_model_request(TurnModelRequestInput {
                turn_id: "later-turn",
                input: std::slice::from_ref(&marker),
                thread_store: &thread,
                turn_store: &later,
            })
            .await?;
        assert_eq!(
            store
                .get::<serde_json::Value>("delivery_attempt", &delivery.id)
                .await?,
            Some(serde_json::to_value(expected_attempt)?)
        );
        assert_eq!(
            store.deliveries(&thread_id).await?[index].1,
            TeamReceipt {
                state: expected_state,
                detail: expected_detail,
                ..initial
            }
        );
    }
    Ok(())
}
