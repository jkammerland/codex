use std::sync::Weak;

use anyhow::Result;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::PreparedTeamHost;
use super::TeamHostService;
use crate::MemberCreateIntent;

#[tokio::test]
async fn departed_creation_stays_released_after_restart_while_unenrolled_creation_stays_held()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let service = TeamHostService::new(
        PreparedTeamHost::load(&config, "installation".into()).await?,
        Weak::new(),
    );
    let store = service.enable().await?;
    let control = TeamControlState {
        coordinator_id: "coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Stopped,
    };
    store.apply_control(&control).await?;

    // These durable intents reproduce startup's post-creation boundary. Only
    // the first root has completed enrollment before leaving the team.
    let departed = ThreadId::new();
    let pending = ThreadId::new();
    for thread_id in [departed, pending] {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let intent = MemberCreateIntent {
            params: TeamMemberCreateParams {
                operation_id: operation_id.clone(),
                coordinator_id: control.coordinator_id.clone(),
                installation_id: "installation".into(),
                cwd: home.path().display().to_string(),
                model: None,
                effort: None,
            },
            thread_id: thread_id.to_string(),
            starting: true,
            outcome: Some(TeamMemberCreateResponse {
                operation_id: operation_id.clone(),
                thread_id: thread_id.to_string(),
                phase: TeamMemberCreatePhase::Created,
                error: None,
            }),
        };
        store
            .put(
                "member_create",
                &format!("coordinator:{operation_id}"),
                &intent,
            )
            .await?;
    }
    store
        .enroll(
            &TeamEnrollment {
                address: TeamAddress {
                    machine_id: "worker".into(),
                    server_id: "primary".into(),
                    thread_id: departed.to_string(),
                },
                alias: "Builder".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            "credential",
        )
        .await?;
    service.apply_gate(&control).await?;
    assert!(service.gate.try_enter(departed).is_none());
    // This fixture has no live Core threads. Model the completed Stop boundary
    // required by leave; the regression concerns the later creation fence.
    store
        .put(
            "control_cleanup_done",
            &format!("{:020}", control.revision),
            &true,
        )
        .await?;
    service
        .leave(TeamLeaveParams {
            coordinator_id: control.coordinator_id,
            thread_id: departed.to_string(),
            operation_id: Some(uuid::Uuid::new_v4().to_string()),
        })
        .await?;
    assert_eq!(store.enrollment(&departed.to_string()).await?, None);
    drop(service);

    let restored = PreparedTeamHost::load(&config, "installation".into()).await?;
    assert_eq!(restored.gate.root(departed), None);
    assert!(restored.gate.try_enter(departed).is_some());
    assert_eq!(restored.gate.root(pending), Some(pending));
    assert!(restored.gate.try_enter(pending).is_none());
    Ok(())
}
