use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn environment_and_approval_notifications_invalidate_passive_discovery_without_a_turn()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(home.path())).await?;
    let address = crate::tests::agent(&coordinator, "reviewer").await?;
    let server = Server {
        id: address.server_id,
        label: "Server".into(),
        endpoint: crate::Endpoint::Local {
            socket: home.path().join("socket"),
        },
        cancellation_adapters: vec![],
        environment_capabilities: Default::default(),
    };
    let machine = Machine {
        id: address.machine_id,
        label: "Machine".into(),
        servers: vec![server.clone()],
    };
    let environment = EnvironmentConnectionNotification {
        thread_id: address.thread_id.clone(),
        environment_id: "remote".into(),
    };
    let (dirty, mut pending) = tokio::sync::watch::channel(0_u64);
    for notification in [
        ServerNotification::EnvironmentConnected(environment.clone()),
        ServerNotification::EnvironmentDisconnected(environment),
        ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
            thread_id: address.thread_id.clone(),
            status: ThreadStatus::Active {
                active_flags: vec![],
            },
        }),
        // Both notifications map to the same legacy "working" text.
        ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
            thread_id: address.thread_id.clone(),
            status: ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
            },
        }),
    ] {
        observe_notification(&coordinator, &machine, &server, notification, &dirty).await?;
        assert!(pending.has_changed()?);
        pending.borrow_and_update();
    }
    assert_eq!(*pending.borrow(), 4);
    assert_eq!(
        coordinator
            .observed_versions
            .lock()
            .await
            .get(&server_key(&machine.id, &server.id)),
        Some(&4)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_activity_changes_before_the_next_host_read() -> Result<()> {
    use crate::model::Agent;
    use crate::model::ServerStatus;
    use crate::model::address_key;
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(home.path())).await?;
    let address = crate::tests::agent(&coordinator, "reviewer").await?;
    let key = address_key(&address);
    let mut agent = coordinator
        .store
        .get::<Agent>("agent", &key)
        .await?
        .unwrap();
    agent.runtime = Some(serde_json::from_value(serde_json::json!({
        "threadId":address.thread_id, "rootThreadId":address.thread_id, "parentThreadId":null,
        "alias":"Reviewer", "loaded":true, "archived":false, "turnId":null,
        "currentModel":null, "currentEffort":null, "nextModel":null, "nextEffort":null, "modelError":null,
        "inFlight":0, "reconciling":false, "manuallyHeld":false, "continuationTurnId":null, "jobs":[], "activity":"idle"
    }))?);
    coordinator.store.put("agent", &key, &agent).await?;
    let server = Server {
        id: address.server_id.clone(),
        label: "Server".into(),
        endpoint: crate::Endpoint::Local {
            socket: home.path().join("socket"),
        },
        cancellation_adapters: vec![],
        environment_capabilities: Default::default(),
    };
    let machine = Machine {
        id: address.machine_id.clone(),
        label: "Machine".into(),
        servers: vec![server.clone()],
    };
    coordinator.statuses.write().await.insert(
        server_key(&machine.id, &server.id),
        ServerStatus {
            machine_id: machine.id.clone(),
            machine_label: machine.label.clone(),
            server_id: server.id.clone(),
            server_label: server.label.clone(),
            installation_id: Some("installation".into()),
            connected: true,
            applied_revision: Some(0),
            error: None,
        },
    );
    let turn = Turn {
        id: "turn".into(),
        root_turn_id: None,
        items: vec![],
        items_view: Default::default(),
        status: TurnStatus::InProgress,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    for (notification, activity, readiness) in [
        (
            ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: address.thread_id.clone(),
                turn: turn.clone(),
            }),
            TeamActivity::Working,
            TeamReadiness::Busy,
        ),
        (
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: address.thread_id.clone(),
                status: ThreadStatus::Active {
                    active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
                },
            }),
            TeamActivity::WaitingForApproval,
            TeamReadiness::Busy,
        ),
        (
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: address.thread_id.clone(),
                status: ThreadStatus::Active {
                    active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
                },
            }),
            TeamActivity::WaitingForInput,
            TeamReadiness::Busy,
        ),
        (
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: address.thread_id.clone(),
                turn: Turn {
                    status: TurnStatus::Completed,
                    ..turn
                },
            }),
            TeamActivity::Idle,
            TeamReadiness::Ready,
        ),
        (
            ServerNotification::ThreadStatusChanged(ThreadStatusChangedNotification {
                thread_id: address.thread_id.clone(),
                status: ThreadStatus::NotLoaded,
            }),
            TeamActivity::Unloaded,
            TeamReadiness::Resumable,
        ),
    ] {
        coordinator.observe(&machine, &server, notification).await?;
        let current = coordinator.agents().await?.remove(0);
        assert_eq!(
            (
                current.discovery.availability.activity,
                current.discovery.availability.readiness
            ),
            (activity, readiness)
        );
    }
    Ok(())
}
