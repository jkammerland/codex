use std::sync::Arc;

use codex_app_server_protocol::*;
use codex_extension_api::ExecutionControl;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

use super::PreparedTeamHost;
use super::TeamHostService;
use crate::delivery::DeliveryBoundary;

#[derive(Default)]
struct RegistrationBarrier {
    entered: Notify,
    release: Notify,
}

impl TurnLifecycleContributor for RegistrationBarrier {
    fn on_turn_start<'a>(&'a self, _input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "barriers reproduce Stop cleanup passing a not-yet-registered turn"
)]
async fn stop_settles_a_delivery_that_registers_after_the_first_cleanup_scan() -> anyhow::Result<()>
{
    let model = responses::start_mock_server().await;
    let barrier = Arc::new(RegistrationBarrier::default());
    let gate = ExecutionControl::default();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.execution_control(gate.clone());
    extensions.turn_lifecycle_contributor(barrier.clone());
    let fixture = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .build(&model)
        .await?;
    let thread_id = fixture.session_configured.thread_id;
    let mut prepared = PreparedTeamHost::load(&fixture.config, "installation".into()).await?;
    prepared.gate = gate.clone();
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

    // Own the ordinary lane as the dispatcher does. Priority Stop must still
    // acknowledge, and its first scan must run before registration is released.
    let _ordinary = service.operations.lock().await;
    let cancellation_barrier = service.job_updates.lock().await;
    let store = service.store.get().unwrap();
    let mut stored = store.subscribe();
    store
        .receive(&TeamDelivery {
            id: "delivery".into(),
            thread_id: thread_id.to_string(),
            mode: TeamDeliveryMode::NextTurn,
            wake: true,
            message: TeamMessage {
                id: "message".into(),
                sender: address.clone(),
                recipient: Some(address),
                kind: TeamMessageKind::Request,
                body: "Do not run after Stop".into(),
                reply_to: None,
                topic: None,
                created_at: 1,
            },
        })
        .await?;
    let dispatch_service = Arc::clone(&service);
    let dispatch = tokio::spawn(async move {
        dispatch_service
            .dispatch_delivery(thread_id, DeliveryBoundary::IdleOrSafeBoundary)
            .await
    });
    barrier.entered.notified().await;
    assert_eq!(fixture.codex.team_active_turn_id().await, None);
    assert!(gate.pending_turn_starts(thread_id) > 0);

    let stopped = service
        .control(TeamControlParams {
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 2,
                mode: TeamExecutionMode::Stopped,
            },
        })
        .await?;
    assert!(!stopped.reconciled);
    while store
        .get::<i64>("stop_settled_revision", &thread_id.to_string())
        .await?
        != Some(2)
    {
        stored.changed().await?;
    }

    // The already admitted input installs its task after that first scan.
    // The closed gate prevents inference, but cleanup must remove the task too.
    barrier.release.notify_one();
    dispatch.await??;
    assert!(fixture.codex.team_active_turn_id().await.is_some());
    drop(cancellation_barrier);
    while !store
        .get::<bool>("control_cleanup_done", "00000000000000000002")
        .await?
        .unwrap_or(false)
    {
        stored.changed().await?;
    }

    assert_eq!(fixture.codex.team_active_turn_id().await, None);
    assert_eq!(gate.pending_turn_starts(thread_id), 0);
    assert_eq!(gate.snapshot(thread_id).unwrap().in_flight, 0);
    assert!(model.received_requests().await.unwrap().is_empty());
    Ok(())
}
