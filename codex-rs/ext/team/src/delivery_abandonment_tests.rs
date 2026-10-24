use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;

use codex_app_server_protocol::*;
use codex_core::StartIfIdleSubmission;
use codex_core::TurnInputRequest;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::OnceCell;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::PreparedTeamHost;
use crate::TeamHostService;
use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::DeliveryAttempt;
use crate::lifecycle::HostEvent;

async fn seed(
    service: &TeamHostService,
    root: ThreadId,
    state: TeamReceiptState,
) -> anyhow::Result<(TeamMemberRetirement, TeamDelivery, TeamReceipt)> {
    let store = service.enable().await?;
    let address = TeamAddress {
        machine_id: "machine".into(),
        server_id: "server".into(),
        thread_id: root.to_string(),
    };
    store
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
    let delivery = TeamDelivery {
        id: "historical-delivery".into(),
        thread_id: root.to_string(),
        mode: TeamDeliveryMode::SafeBoundary,
        wake: true,
        message: TeamMessage {
            id: "historical-message".into(),
            sender: address.clone(),
            recipient: Some(address.clone()),
            kind: TeamMessageKind::Request,
            body: "original admitted request".into(),
            reply_to: None,
            topic: None,
            created_at: 1,
        },
    };
    let receipt = TeamReceipt {
        state,
        turn_id: Some("historical-turn".into()),
        detail: Some("original outcome is uncertain".into()),
        ..store.receive(&delivery).await?
    };
    store.update_receipt(&receipt).await?;
    Ok((
        TeamMemberRetirement {
            operation_id: uuid::Uuid::new_v4().to_string(),
            address,
            state: TeamMemberRetirementState::Retiring,
            waiting_reason: None,
        },
        delivery,
        receipt,
    ))
}

fn params(thread_id: ThreadId) -> TeamDeliveryAbandonParams {
    TeamDeliveryAbandonParams {
        operation_id: uuid::Uuid::new_v4().to_string(),
        coordinator_id: "coordinator".into(),
        thread_id: thread_id.to_string(),
        delivery_id: "historical-delivery".into(),
        reason: "Reviewed the failed request; remove this member without replay.".into(),
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "exercise retirement transitions under their required host operations fence"
)]
async fn abandonment_resolves_stuck_retirement_without_rewriting_failed_or_legacy_evidence()
-> anyhow::Result<()> {
    // Accepted legacy input has no dispatch/terminal proof. Both this uncertainty
    // and a proven failed participant require the same explicit operator decision.
    for state in [
        TeamReceiptState::Accepted,
        TeamReceiptState::NeedsReconciliation,
    ] {
        let home = tempfile::tempdir()?;
        let config = core_test_support::load_default_config_for_test(&home).await;
        let prepared = PreparedTeamHost::load(&config, "installation".into()).await?;
        let service = TeamHostService::new(prepared, Weak::new());
        let root = ThreadId::new();
        let (intent, delivery, receipt) = seed(&service, root, state).await?;
        let store = service.store.get().unwrap();
        if state == TeamReceiptState::NeedsReconciliation {
            store
                .put(
                    "delivery_attempt",
                    &delivery.id,
                    &DeliveryAttempt {
                        id: "original-dispatch".into(),
                        generation: "original-generation".into(),
                        admission_revision: 1,
                        turn_id: "historical-turn".into(),
                        acknowledged: true,
                        participation: BTreeMap::from([(
                            "original-runtime".into(),
                            Some(AttemptOutcome::Failed("provider error".into())),
                        )]),
                        provider_acknowledgements: BTreeMap::from([(
                            "original-runtime".into(),
                            None,
                        )]),
                        terminal_outcomes: BTreeMap::from([(
                            "original-runtime".into(),
                            AttemptOutcome::Failed("provider error".into()),
                        )]),
                    },
                )
                .await?;
        }
        let attempt = store
            .get::<serde_json::Value>("delivery_attempt", &delivery.id)
            .await?;
        {
            let _operation = service.operations.lock().await;
            service.apply_retirement(&intent).await?;
            service.advance_retirements().await?;
            let retiring = service.retirement_states().await?.remove(0);
            assert_eq!(retiring.state, TeamMemberRetirementState::Retiring);
            assert!(
                retiring
                    .waiting_reason
                    .unwrap()
                    .contains("admitted delivery")
            );
        }

        let request = params(root);
        let response = service.abandon_delivery(request.clone()).await?;
        let expected = TeamDeliveryAbandonResponse {
            abandonment: TeamDeliveryAbandonment {
                operation_id: request.operation_id.clone(),
                delivery_id: delivery.id.clone(),
                thread_id: root.to_string(),
                reason: request.reason.clone(),
            },
        };
        assert_eq!(response, expected);
        assert_eq!(
            service.retirement_states().await?[0].state,
            TeamMemberRetirementState::Removed
        );
        assert_eq!(service.gate.root(root), None);
        assert_eq!(store.enrollment(&root.to_string()).await?, None);
        assert_eq!(
            store.deliveries(&root.to_string()).await?,
            vec![(delivery.clone(), receipt.clone())]
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("delivery_attempt", &delivery.id)
                .await?,
            attempt
        );

        // A lost response remains recoverable after enrollment and gate removal.
        assert_eq!(service.abandon_delivery(request.clone()).await?, expected);
        let mut conflict = request;
        conflict.reason = "a different decision".into();
        assert!(
            service
                .abandon_delivery(conflict)
                .await
                .unwrap_err()
                .to_string()
                .contains("conflicting content")
        );
        let events = store
            .outbox(0, 100)
            .await?
            .into_iter()
            .filter(|entry| entry.kind == "delivery_abandonment")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload,
            serde_json::to_value(&expected.abandonment)?
        );

        let status = service
            .agent_message_status(
                &intent.address,
                TeamMessageStatusParams {
                    message_id: Some(delivery.message.id),
                    cursor: None,
                    limit: None,
                },
            )
            .await?;
        assert_eq!(status.data[0].recipients[0].receipt, Some(receipt));
        assert_eq!(
            status.data[0].recipients[0].abandonment,
            Some(expected.abandonment)
        );
    }
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "fixture evidence updates serialize with recovery under the host operations fence"
)]
async fn abandonment_rejects_wrong_ownership_address_nonadmitted_input_and_unsettled_work()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let prepared = PreparedTeamHost::load(&config, "installation".into()).await?;
    let service = TeamHostService::new(prepared, Weak::new());
    let root = ThreadId::new();
    let (intent, delivery, receipt) = seed(&service, root, TeamReceiptState::Accepted).await?;
    let store = service.store.get().unwrap();
    let request = params(root);
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("not retiring")
    );
    service.apply_retirement(&intent).await?;

    // An unresolved second delivery keeps retirement pending while rejected
    // states are exercised below, independent of background host scheduling.
    let mut mismatched = delivery.clone();
    mismatched.id = "wrong-address".into();
    mismatched.message.id = "wrong-address-message".into();
    mismatched.message.recipient.as_mut().unwrap().server_id = "other-server".into();
    let mismatched_receipt = TeamReceipt {
        state: TeamReceiptState::Accepted,
        ..store.receive(&mismatched).await?
    };
    store.update_receipt(&mismatched_receipt).await?;
    let mut wrong = request.clone();
    wrong.delivery_id = mismatched.id;
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("recipient address")
    );

    let mut wrong = request.clone();
    wrong.coordinator_id = "other".into();
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("coordinator identity")
    );
    let mut wrong = request.clone();
    wrong.thread_id = ThreadId::new().to_string();
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("not enrolled")
    );
    let mut wrong = request.clone();
    wrong.delivery_id = "different-delivery".into();
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("not addressed")
    );
    let mut wrong = request.clone();
    wrong.reason = " \n ".into();
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("reason")
    );
    let mut wrong = request.clone();
    wrong.operation_id = uuid::Uuid::parse_str(&wrong.operation_id)?
        .simple()
        .to_string();
    assert!(
        service
            .abandon_delivery(wrong)
            .await
            .unwrap_err()
            .to_string()
            .contains("canonical UUID")
    );

    let work = service.gate.try_enter(root).unwrap();
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("admitted work")
    );
    drop(work);
    service.gate.hold_manually(root);
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("operator hold")
    );
    assert!(service.gate.is_manually_held(root));
    service.gate.resume_for_user(root);

    // Continuations and jobs are family blockers, even when the addressed root
    // itself is idle. They are left intact for their existing operator controls.
    let descendant = ThreadId::new();
    service.gate.inherit(descendant, root);
    store.put("member", &descendant.to_string(), &serde_json::json!({
        "threadId": descendant.to_string(), "rootThreadId": root.to_string(),
        "parentThreadId": root.to_string(), "alias":"child", "loaded":false, "archived":false,
        "inFlight":0, "reconciling":false, "manuallyHeld":false, "jobs":[],
    })).await?;
    store
        .put(
            "continuation",
            &descendant.to_string(),
            &serde_json::json!({
                "turn_id":"interrupted", "generation":"generation", "history_offset":0,
                "cyber_access_program":null,
            }),
        )
        .await?;
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("interrupted continuation")
    );
    store
        .delete("continuation", &descendant.to_string())
        .await?;
    store
        .put(
            "job",
            "running",
            &crate::jobs::StoredJob {
                job: TeamTrackedJob {
                    id: "running".into(),
                    thread_id: descendant.to_string(),
                    server: "jobs".into(),
                    tool: "launch".into(),
                    handle: Some("job".into()),
                    status: TeamJobStatus::Running,
                    detail: None,
                },
                adapter: None,
                launch_revision: None,
                requested_revision: None,
                attempted_revision: None,
                registration_fingerprint: None,
            },
        )
        .await?;
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("external jobs")
    );
    assert_eq!(
        service.jobs(&descendant.to_string()).await?[0].status,
        TeamJobStatus::Running
    );
    store.delete("job", "running").await?;

    // A stale unresolved receipt cannot discard positive completion evidence.
    store.put("delivery_attempt", &delivery.id, &serde_json::json!({
        "id":"dispatch", "generation":"generation", "admission_revision":1,
        "turn_id":"historical-turn", "acknowledged":true,
        "participation":{"runtime":"Completed"}, "terminal_outcomes":{"runtime":"Completed"},
    })).await?;
    assert!(
        service
            .abandon_delivery(request.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("completed execution evidence")
    );
    store.delete("delivery_attempt", &delivery.id).await?;

    for state in [
        TeamReceiptState::Pending,
        TeamReceiptState::Completed,
        TeamReceiptState::Interrupted,
        TeamReceiptState::Rejected,
    ] {
        let _operation = service.operations.lock().await;
        store
            .update_receipt(&TeamReceipt {
                state,
                ..receipt.clone()
            })
            .await?;
        drop(_operation);
        assert!(
            service
                .abandon_delivery(request.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("unresolved outcome")
        );
    }
    assert_eq!(
        store
            .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
            .await?,
        None
    );
    Ok(())
}

struct FinishingGate {
    reached: Notify,
    release: Semaphore,
}

impl TurnLifecycleContributor for FinishingGate {
    fn on_turn_stop<'a>(&'a self, _input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.reached.notify_one();
            let _release = self.release.acquire().await.unwrap();
        })
    }
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "exercise retirement transitions under their required host operations fence"
)]
async fn abandonment_and_retirement_wait_for_active_and_finishing_turns() -> anyhow::Result<()> {
    let (release_response, response_gate) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(response_gate),
        body: responses::sse_completed("current-turn"),
    }]])
    .await;
    let gate = ExecutionControl::default();
    let finishing = Arc::new(FinishingGate {
        reached: Notify::new(),
        release: Semaphore::new(0),
    });
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    extensions.turn_lifecycle_contributor(finishing.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let fixture = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&model)
        .await?;
    let root = fixture.session_configured.thread_id;
    let mut prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    prepared.gate = gate;
    let service = TeamHostService::new(prepared, Arc::downgrade(&fixture.thread_manager));
    let (intent, delivery, receipt) =
        seed(&service, root, TeamReceiptState::NeedsReconciliation).await?;
    let StartIfIdleSubmission::Started { .. } = fixture
        .codex
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "a separate ordinary turn".into(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        anyhow::bail!("ordinary turn must start")
    };
    model.wait_for_request_count(1).await;
    service.apply_retirement(&intent).await?;
    assert!(fixture.codex.team_has_active_or_finishing_turn().await);
    let error = service.abandon_delivery(params(root)).await.unwrap_err();
    assert!(
        error.to_string().contains("admitted work"),
        "{error:#}; gate: {:?}",
        service.gate.snapshot(root)
    );

    release_response.send(()).unwrap();
    finishing.reached.notified().await;
    assert_eq!(fixture.codex.team_active_turn_id().await, None);
    assert!(fixture.codex.team_has_active_or_finishing_turn().await);
    let error = service.abandon_delivery(params(root)).await.unwrap_err();
    assert!(
        error.to_string().contains("active or finishing"),
        "{error:#}"
    );
    {
        let _operation = service.operations.lock().await;
        service.advance_retirements().await?;
        assert_eq!(
            service.retirement_states().await?[0].state,
            TeamMemberRetirementState::Retiring
        );
    }
    assert_eq!(
        service
            .store
            .get()
            .unwrap()
            .deliveries(&root.to_string())
            .await?,
        vec![(delivery, receipt)]
    );
    finishing.release.add_permits(1);
    ThreadIdle::wait(&fixture.codex).await;
    assert!(!fixture.codex.team_has_active_or_finishing_turn().await);
    service.abandon_delivery(params(root)).await?;
    assert_eq!(service.gate.root(root), None);
    fixture.codex.shutdown_and_wait().await?;
    model.shutdown().await;
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "replay the worker terminal handler while holding its required host operations fence"
)]
async fn pending_family_terminal_evidence_must_settle_before_abandonment() -> anyhow::Result<()> {
    for outcome in [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed("provider failure".into()),
    ] {
        let home = tempfile::tempdir()?;
        let config = core_test_support::load_default_config_for_test(&home).await;
        let prepared = PreparedTeamHost::load(&config, "installation".into()).await?;
        let (events, _event_receiver) = mpsc::unbounded_channel();
        let (control_events, _control_receiver) = mpsc::unbounded_channel();

        // Own the host lane explicitly: the core has already drained, while
        // its persisted terminal event cannot be consumed by a background task.
        let service = TeamHostService {
            cancellations: Arc::new(Mutex::new(Default::default())),
            delivery_updates: Mutex::new(()),
            job_updates: Arc::new(Mutex::new(())),
            gate: prepared.gate,
            store: OnceCell::new_with(prepared.store),
            manager: Weak::new(),
            operations: Mutex::new(()),
            control_updates: Mutex::new(()),
            model_updates: Mutex::new(()),
            member_create_updates: Mutex::new(Default::default()),
            confirmed_control_revision: std::sync::atomic::AtomicI64::new(-1),
            control_events,
            events,
            changed: watch::channel(0).0,
            path: prepared.path,
            sqlite: prepared.sqlite,
            installation_id: prepared.installation_id,
        };
        let root = ThreadId::new();
        let (intent, delivery, receipt) = seed(&service, root, TeamReceiptState::Accepted).await?;
        let store = service.store.get().unwrap();
        let request = params(root);
        let attempt = DeliveryAttempt {
            id: "original-dispatch".into(),
            generation: "original-generation".into(),
            admission_revision: 1,
            turn_id: "historical-turn".into(),
            acknowledged: true,
            participation: BTreeMap::from([("terminal-runtime".into(), None)]),
            provider_acknowledgements: BTreeMap::new(),
            terminal_outcomes: BTreeMap::new(),
        };
        store
            .put("delivery_attempt", &delivery.id, &attempt)
            .await?;
        service.apply_retirement(&intent).await?;
        let unrelated_event = HostEvent::Finished {
            thread_id: ThreadId::new().to_string(),
            turn_id: "other-turn".into(),
            interrupted: false,
            runtime: "other-runtime".into(),
            error: None,
        };
        store
            .put("lifecycle_finished", "other-runtime", &unrelated_event)
            .await?;
        let event = HostEvent::Finished {
            thread_id: root.to_string(),
            turn_id: "historical-turn".into(),
            interrupted: false,
            runtime: "terminal-runtime".into(),
            error: match &outcome {
                AttemptOutcome::Failed(error) => Some(error.clone()),
                AttemptOutcome::Completed => None,
                AttemptOutcome::Interrupted => {
                    unreachable!("only successful and failed terminals are exercised")
                }
            },
        };
        store
            .put("lifecycle_finished", "terminal-runtime", &event)
            .await?;

        let error = service.abandon_delivery(request.clone()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Waiting for terminal delivery evidence to settle"),
            "{error:#}"
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("lifecycle_finished", "terminal-runtime")
                .await?,
            Some(serde_json::to_value(event)?)
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("delivery_attempt", &delivery.id)
                .await?,
            Some(serde_json::to_value(&attempt)?)
        );
        assert_eq!(
            store.deliveries(&root.to_string()).await?,
            vec![(delivery.clone(), receipt)]
        );
        assert_eq!(
            store
                .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                .await?,
            None
        );
        assert_eq!(
            store
                .host_operation_result::<TeamDeliveryAbandonResponse>(
                    &format!("coordinator:{}", request.operation_id),
                    &serde_json::to_string(&request)?
                )
                .await?,
            None
        );

        // These are the worker's normal evidence transition and journal ACK.
        // The recovery RPC does neither while the terminal handler is pending.
        {
            let _operation = service.operations.lock().await;
            service
                .finish_deliveries(
                    &root.to_string(),
                    "historical-turn",
                    "terminal-runtime",
                    outcome.clone(),
                )
                .await?;
            store
                .delete("lifecycle_finished", "terminal-runtime")
                .await?;
        }
        let settled_receipts = store.deliveries(&root.to_string()).await?;
        let settled_attempt = store
            .get::<serde_json::Value>("delivery_attempt", &delivery.id)
            .await?;

        // A descendant's unconsumed terminal also fences the whole family,
        // even after the addressed delivery's own evidence has settled.
        let descendant = ThreadId::new();
        service.gate.inherit(descendant, root);
        store.put("member", &descendant.to_string(), &serde_json::json!({
            "threadId":descendant.to_string(), "rootThreadId":root.to_string(),
            "parentThreadId":root.to_string(), "alias":"child", "loaded":false, "archived":false,
            "inFlight":0, "reconciling":false, "manuallyHeld":false, "jobs":[],
        })).await?;
        let child_event = HostEvent::Finished {
            thread_id: descendant.to_string(),
            turn_id: "child-turn".into(),
            interrupted: false,
            runtime: "child-runtime".into(),
            error: None,
        };
        store
            .put("lifecycle_finished", "child-runtime", &child_event)
            .await?;
        let error = service.abandon_delivery(request.clone()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Waiting for terminal delivery evidence to settle"),
            "{error:#}"
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("lifecycle_finished", "child-runtime")
                .await?,
            Some(serde_json::to_value(child_event)?)
        );
        store.delete("lifecycle_finished", "child-runtime").await?;

        match outcome {
            AttemptOutcome::Completed => {
                assert_eq!(settled_receipts[0].1.state, TeamReceiptState::Completed);
                assert!(
                    service
                        .abandon_delivery(request)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("unresolved outcome")
                );
                assert_eq!(
                    store
                        .get::<TeamDeliveryAbandonment>("delivery_abandonment", &delivery.id)
                        .await?,
                    None
                );
            }
            AttemptOutcome::Failed(_) => {
                assert_eq!(
                    settled_receipts[0].1.state,
                    TeamReceiptState::NeedsReconciliation
                );
                service.abandon_delivery(request).await?;
                assert_eq!(service.gate.root(root), None);
            }
            AttemptOutcome::Interrupted => {
                unreachable!("only successful and failed terminals are exercised")
            }
        }
        assert_eq!(store.deliveries(&root.to_string()).await?, settled_receipts);
        assert_eq!(
            store
                .get::<serde_json::Value>("delivery_attempt", &delivery.id)
                .await?,
            settled_attempt
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("lifecycle_finished", "other-runtime")
                .await?,
            Some(serde_json::to_value(unrelated_event)?)
        );
    }
    Ok(())
}
