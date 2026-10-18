use super::*;
use pretty_assertions::assert_eq;

fn fixture() -> (Agent, ServerStatus, TeamControlState) {
    let runtime: TeamAgentRuntime = serde_json::from_value(serde_json::json!({
        "threadId":"00000000-0000-7000-8000-000000000001", "rootThreadId":"00000000-0000-7000-8000-000000000001", "parentThreadId":null,
        "alias":"Builder", "loaded":true, "archived":false, "turnId":null,
        "currentModel":null, "currentEffort":null, "nextModel":null, "nextEffort":null, "modelError":null,
        "inFlight":0, "reconciling":false, "manuallyHeld":false, "continuationTurnId":null, "jobs":[], "activity":"idle",
        "environments":[{"id":"selected", "current":false, "next":true, "os":null, "shell":null, "declaredCapabilities":[]}]
    })).unwrap();
    let agent = Agent {
        discovery: Default::default(),
        enrollment: TeamEnrollment {
            address: TeamAddress {
                machine_id: "m".into(),
                server_id: "s".into(),
                thread_id: runtime.thread_id.clone(),
            },
            alias: "Builder".into(),
            manually_held: false,
            continuation_turn_id: None,
        },
        runtime: Some(runtime),
        status: "idle".into(),
        current_model: None,
        current_effort: None,
        next_model: None,
        next_effort: None,
        project: None,
        requested_selection: None,
    };
    let status = ServerStatus {
        machine_id: "m".into(),
        machine_label: "Machine".into(),
        server_id: "s".into(),
        server_label: "Server".into(),
        installation_id: Some("installation".into()),
        connected: true,
        applied_revision: Some(7),
        error: None,
    };
    (
        agent,
        status,
        TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 7,
            mode: TeamExecutionMode::Running,
        },
    )
}

#[test]
fn activity_readiness_holds_and_unknown_environment_are_independent() {
    let (mut agent, mut server, control) = fixture();
    let declarations = std::collections::BTreeMap::from([
        ("selected".into(), vec!["msvc".into()]),
        ("unselected".into(), vec!["cuda".into()]),
    ]);
    let discovery = project_discovery(&agent, Some(&server), &control, Some(&declarations));
    assert_eq!(
        discovery,
        TeamAgentDiscovery {
            availability: TeamAvailability {
                synchronization: TeamSynchronization::Current,
                activity: TeamActivity::Idle,
                readiness: TeamReadiness::Ready,
                holds: vec![],
                reasons: vec![]
            },
            environments: vec![TeamExecutionEnvironment {
                id: "selected".into(),
                current: false,
                next: true,
                os: None,
                shell: None,
                declared_capabilities: vec!["msvc".into()]
            }]
        }
    );
    // Family admission counters include siblings and do not measure this agent's activity.
    agent.runtime.as_mut().unwrap().in_flight = 1;
    assert_eq!(
        project_discovery(&agent, Some(&server), &control, None)
            .availability
            .readiness,
        TeamReadiness::Ready
    );
    agent.runtime.as_mut().unwrap().loaded = false;
    assert_eq!(
        project_discovery(&agent, Some(&server), &control, None)
            .availability
            .readiness,
        TeamReadiness::Resumable
    );
    agent.enrollment.manually_held = true;
    assert_eq!(
        project_discovery(&agent, Some(&server), &control, None)
            .availability
            .readiness,
        TeamReadiness::Held
    );
    server.connected = false;
    let unavailable = project_discovery(&agent, Some(&server), &control, None).availability;
    assert_eq!(unavailable.readiness, TeamReadiness::Unknown);
    assert_eq!(unavailable.holds, vec![TeamExecutionHold::Manual]);
}

#[tokio::test]
async fn directory_replaces_departed_entries_and_reuses_unchanged_revision() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let (agent, _, _) = fixture();
    coordinator
        .store
        .put("agent", &address_key(&agent.enrollment.address), &agent)
        .await?;
    let first = coordinator.directory_snapshot().await?;
    assert_eq!(first.data.len(), 1);
    assert_eq!(coordinator.directory_snapshot().await?, first);
    coordinator
        .store
        .delete("agent", &address_key(&agent.enrollment.address))
        .await?;
    let next = coordinator.directory_snapshot().await?;
    assert_eq!(next.revision, first.revision + 1);
    assert_eq!(next.data, vec![]);
    Ok(())
}
