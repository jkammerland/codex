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
