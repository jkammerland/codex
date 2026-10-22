use anyhow::Result;
use clap::Parser;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::*;

fn member_request() -> TeamMemberCreateRequest {
    TeamMemberCreateRequest {
        machine_id: "worker".into(),
        server_id: "primary".into(),
        alias: "Builder".into(),
        cwd: "/checkout".into(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some("medium".into()),
    }
}

#[tokio::test]
async fn member_acceptance_reserves_alias_and_survives_restart_without_a_worker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut manifest = tests::manifest(directory.path());
    manifest.machines.push(Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![Server {
            id: "primary".into(),
            label: "Primary".into(),
            endpoint: Endpoint::Local {
                socket: directory.path().join("unavailable.sock"),
            },
            environment_capabilities: Default::default(),
            cancellation_adapters: Vec::new(),
        }],
    });
    let coordinator = Coordinator::open(manifest.clone()).await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        })
        .await?;
    let control = coordinator.current_control().await?;
    let operation = TeamOperation::new(TeamRequest::CreateMember {
        request: member_request(),
    });
    let TeamResponse::Operation {
        operation: TeamOperatorOperation::MemberCreate(accepted),
    } = coordinator.execute_operation(operation.clone()).await?
    else {
        panic!("creation accepted")
    };
    assert_eq!(accepted.phase, TeamMemberCreateState::Creating);
    assert_eq!(accepted.address, None);
    assert_eq!(coordinator.current_control().await?, control);
    assert!(coordinator.agents().await?.is_empty());
    assert!(
        coordinator
            .execute_operation(TeamOperation::new(operation.request.clone()))
            .await
            .is_err()
    );
    assert!(
        coordinator
            .ensure_creation_alias_available("Builder", None)
            .await
            .is_err()
    );
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    let TeamResponse::Operation {
        operation: replayed,
    } = restored.execute_operation(operation.clone()).await?
    else {
        panic!("creation replayed")
    };
    assert_eq!(replayed, TeamOperatorOperation::MemberCreate(accepted));
    assert_eq!(restored.snapshot().await?.operations.len(), 1);
    assert!(
        restored
            .execute_operation(TeamOperation {
                operation_id: operation.operation_id,
                request: TeamRequest::Control {
                    mode: TeamExecutionMode::Running
                }
            })
            .await
            .is_err()
    );
    assert_eq!(restored.current_control().await?, control);
    Ok(())
}

async fn legacy_post(
    coordinator: &Coordinator,
    topic: &str,
    body: &str,
) -> Result<(TeamOperation, String)> {
    let operation = TeamOperation::new(TeamRequest::Post {
        topic: topic.into(),
        body: body.into(),
    });
    let TeamResponse::Message { message } =
        coordinator.execute_operation(operation.clone()).await?
    else {
        panic!("message")
    };
    Ok((operation, message.message.id))
}

#[tokio::test]
async fn purge_removes_whole_conversations_and_old_retry_bodies_without_replaying_new_posts()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = tests::manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let (original, root) = legacy_post(&coordinator, "trial", "private root text").await?;
    coordinator
        .execute_operation(TeamOperation::new(TeamRequest::Reply {
            message_id: root.clone(),
            body: "private reply text".into(),
            delivery: TeamDeliveryMode::NextTurn,
        }))
        .await?;
    let retry = TeamOperation::new(TeamRequest::Retry {
        message_id: root.clone(),
    });
    coordinator.execute_operation(retry.clone()).await?;
    let (_, untouched) = legacy_post(&coordinator, "keep", "keep this conversation").await?;
    coordinator
        .store
        .put("installation", "offline/primary", &"offline-installation")
        .await?;
    let scope = TeamBoardPurgeScope::Topic {
        topic: "trial".into(),
    };
    let preview = coordinator.preview_board_purge(&scope).await?;
    assert_eq!(preview.conversation_count, 1);
    assert_eq!(preview.message_count, 2);
    let purge = TeamOperation::new(TeamRequest::BoardPurge {
        scope: scope.clone(),
        expected_revision: preview.revision,
    });
    let TeamResponse::Operation {
        operation: TeamOperatorOperation::BoardPurge(removed),
    } = coordinator.execute_operation(purge.clone()).await?
    else {
        panic!("purge")
    };
    assert_eq!(removed.workers.len(), 1);
    assert!(!removed.workers[0].acknowledged);
    let snapshot = coordinator.snapshot().await?;
    assert_eq!(
        snapshot
            .board
            .entries
            .iter()
            .map(|entry| entry.message.id.clone())
            .collect::<Vec<_>>(),
        vec![untouched.clone()]
    );
    for namespace in [
        "board_entry",
        "message",
        "operator_result",
        "rejected_outbox",
    ] {
        let records = coordinator
            .store
            .list::<serde_json::Value>(namespace, "", u32::MAX)
            .await?;
        let encoded = serde_json::to_string(&records)?;
        assert!(
            !encoded.contains("private root text"),
            "{namespace} retained root content"
        );
        assert!(
            !encoded.contains("private reply text"),
            "{namespace} retained reply content"
        );
    }
    for operation in [original, retry] {
        assert!(
            matches!(coordinator.execute_operation(operation).await?, TeamResponse::Removed { message_id } if message_id == root)
        );
    }
    assert!(
        coordinator
            .execute_operation(TeamOperation::new(TeamRequest::Reply {
                message_id: root.clone(),
                body: "late reply".into(),
                delivery: TeamDeliveryMode::NextTurn,
            }))
            .await
            .is_err()
    );
    let (_, later) = legacy_post(&coordinator, "trial", "after cleanup").await?;
    coordinator.execute_operation(purge).await?;
    drop(coordinator);

    let restored = Coordinator::open(manifest).await?;
    let remaining = restored
        .snapshot()
        .await?
        .board
        .entries
        .into_iter()
        .map(|entry| entry.message.id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(remaining, [untouched, later].into_iter().collect());
    assert!(restored.store.board_message_removed(&root).await?);
    Ok(())
}

fn purge_args(arguments: &[&str]) -> BoardPurgeArgs {
    let mut args = vec!["agents", "board", "purge"];
    args.extend_from_slice(arguments);
    let command = TeamCommand::try_parse_from(args).unwrap();
    let TeamAction::Board(BoardCommand {
        action: BoardAction::Purge(args),
    }) = command.command
    else {
        panic!("offline command")
    };
    args
}

#[tokio::test]
async fn offline_preview_requires_exclusive_owner_and_apply_requires_reviewed_revision()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = tests::manifest(directory.path());
    let path = directory.path().join("team.toml");
    tokio::fs::write(&path, toml::to_string(&manifest)?).await?;
    let coordinator = Coordinator::open(manifest.clone()).await?;
    legacy_post(&coordinator, "trial", "remove me").await?;
    assert!(
        maintenance_cli::run(&path, purge_args(&["--all", "--offline"]), None)
            .await
            .is_err()
    );
    let clock = coordinator.store.board_clock().await?;
    drop(coordinator);
    let TeamResponse::BoardPurgePreview { preview } =
        maintenance_cli::run(&path, purge_args(&["--all", "--offline"]), None).await?
    else {
        panic!("preview")
    };
    assert_eq!(preview.revision, clock.revision);
    assert_eq!(preview.message_count, 1);
    assert!(
        maintenance_cli::run(
            &path,
            purge_args(&["--all", "--offline", "--apply", "--expect-revision", "0"]),
            Some(Uuid::new_v4().to_string())
        )
        .await
        .is_err()
    );
    let restored = Coordinator::open(manifest).await?;
    assert_eq!(restored.snapshot().await?.board.entries.len(), 1);
    assert_eq!(restored.store.board_clock().await?.revision, clock.revision);
    Ok(())
}

#[test]
fn offline_cli_rejects_ambiguous_scopes_and_incomplete_apply() {
    for args in [
        vec!["agents", "board", "purge", "--all"],
        vec![
            "agents",
            "board",
            "purge",
            "--all",
            "--topic",
            "trial",
            "--offline",
        ],
        vec!["agents", "board", "purge", "--all", "--offline", "--apply"],
    ] {
        assert!(TeamCommand::try_parse_from(args).is_err());
    }
}
