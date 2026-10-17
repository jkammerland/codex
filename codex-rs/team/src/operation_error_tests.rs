use anyhow::Context;
use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::routing::post;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use crate::TeamClient;
use crate::TeamOperation;
use crate::TeamOperationRejected;
use crate::TeamRequest;
use crate::TeamResponse;
use crate::coordinator::Coordinator;

fn board_post(topic: &str, body: &str) -> TeamOperation {
    TeamOperation::new(TeamRequest::Board {
        request: TeamBoardParams::Post {
            topic: topic.into(),
            body: body.into(),
            mentions: Vec::new(),
        },
    })
}

#[tokio::test]
async fn board_http_client_preserves_definitive_rejection_and_allows_corrected_submission()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = crate::http::router(coordinator.clone(), "operator-token".into());
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let result: Result<()> = async {
        let client = TeamClient::new(&url, "operator-token".into())?;
        let before = coordinator.board_snapshot().await?;
        let invalid = board_post("", "Keep this draft");
        let error = client.request_operation(&invalid).await.unwrap_err();
        let rejected = error
            .downcast_ref::<TeamOperationRejected>()
            .context("expected typed pre-write rejection")?;
        assert_eq!(
            rejected,
            &TeamOperationRejected {
                operation_id: invalid.operation_id.clone(),
                reason: "board posts require a topic and no recipient".into(),
            }
        );
        assert_eq!(coordinator.board_snapshot().await?, before);
        assert_eq!(
            coordinator
                .store
                .get::<TeamOperationRejected>("operator_rejection", &invalid.operation_id)
                .await?,
            Some(rejected.clone())
        );

        // A selection can become stale while the composer remains open.
        let departed = crate::tests::agent(&coordinator, "Reviewer").await?;
        coordinator
            .forget_agents(&std::collections::HashSet::from([
                crate::model::address_key(&departed),
            ]))
            .await?;
        let stale_mention = TeamOperation::new(TeamRequest::Board {
            request: TeamBoardParams::Post {
                topic: "general".into(),
                body: "@Reviewer".into(),
                mentions: vec![TeamBoardMention {
                    identity: TeamBoardIdentity::Agent { address: departed },
                    display: "@Reviewer".into(),
                    start: 0,
                    end: 9,
                }],
            },
        });
        let error = client.request_operation(&stale_mention).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<TeamOperationRejected>()
                .context("stale mention rejection")?
                .reason,
            "board agent is not enrolled"
        );
        assert_eq!(coordinator.board_snapshot().await?, before);

        let corrected = board_post("general", "Keep this draft");
        let response = client.request_operation(&corrected).await?;
        assert!(matches!(
            response,
            TeamResponse::Board {
                response: TeamBoardResponse::Mutation {
                    state: TeamBoardMutationState::Applied,
                    ..
                }
            }
        ));
        let snapshot = coordinator.board_snapshot().await?;
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].message.id, corrected.operation_id);
        assert_eq!(snapshot.entries[0].message.body, "Keep this draft");

        // A failed legacy command keeps its historical error classification.
        let legacy = TeamOperation::new(TeamRequest::Post {
            topic: String::new(),
            body: "legacy".into(),
        });
        assert!(
            client
                .request_operation(&legacy)
                .await
                .unwrap_err()
                .downcast_ref::<TeamOperationRejected>()
                .is_none()
        );

        // A conflicting retry may already have committed; it is never advertised
        // as a pre-write rejection even if its new content would be invalid.
        let conflict = TeamOperation {
            operation_id: corrected.operation_id,
            request: invalid.request,
        };
        assert!(
            client
                .request_operation(&conflict)
                .await
                .unwrap_err()
                .downcast_ref::<TeamOperationRejected>()
                .is_none()
        );
        assert_eq!(coordinator.board_snapshot().await?, snapshot);
        Ok(())
    }
    .await;
    let _ = shutdown.send(());
    server.await??;
    result
}

#[tokio::test]
async fn board_committed_but_invalid_http_reply_stays_unknown_and_retries_original_identity()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let broken_url = format!("http://{}", listener.local_addr()?);
    let target = coordinator.clone();
    let broken = Router::new().route(
        "/rpc",
        post(move |Json(operation): Json<TeamOperation>| {
            let target = target.clone();
            async move {
                target
                    .execute_operation(operation)
                    .await
                    .expect("test operation commits");
                // The server committed the effect, but a complete valid response did
                // not reach the client. It must retain the original operation ID.
                "{"
            }
        }),
    );
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, broken)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let operation = board_post("general", "Committed before reply failure");
    let result: Result<()> = async {
        let client = TeamClient::new(&broken_url, "operator-token".into())?;
        let error = client.request_operation(&operation).await.unwrap_err();
        assert!(error.downcast_ref::<TeamOperationRejected>().is_none());
        assert!(error.to_string().contains("response was lost or invalid"));
        let before = coordinator.board_snapshot().await?;
        assert_eq!(before.entries.len(), 1);
        assert_eq!(before.entries[0].message.id, operation.operation_id);
        let response = coordinator.execute_operation(operation.clone()).await?;
        assert!(matches!(
            response,
            TeamResponse::Board {
                response: TeamBoardResponse::Mutation {
                    state: TeamBoardMutationState::Applied,
                    ..
                }
            }
        ));
        assert_eq!(coordinator.board_snapshot().await?, before);
        Ok(())
    }
    .await;
    let _ = shutdown.send(());
    server.await??;
    result
}

#[tokio::test]
async fn board_http_transport_loss_is_not_a_definitive_rejection() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        drop(stream);
        Ok::<_, anyhow::Error>(())
    });
    let client = TeamClient::new(&url, "operator-token".into())?;
    let operation = board_post("general", "Unknown delivery");
    let error = client.request_operation(&operation).await.unwrap_err();
    server.await??;
    assert!(error.downcast_ref::<TeamOperationRejected>().is_none());
    assert!(error.to_string().contains(&operation.operation_id));
    Ok(())
}

#[tokio::test]
async fn board_rejection_is_terminal_after_membership_changes_concurrent_retries_and_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest = crate::tests::manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    let address = crate::tests::agent(&coordinator, "Reviewer").await?;
    let key = crate::model::address_key(&address);
    let enrolled = coordinator
        .store
        .get::<crate::Agent>("agent", &key)
        .await?
        .context("enrolled fixture")?;
    coordinator
        .forget_agents(&std::collections::HashSet::from([key.clone()]))
        .await?;
    let operation = TeamOperation::new(TeamRequest::Board {
        request: TeamBoardParams::Post {
            topic: "general".into(),
            body: "@Reviewer".into(),
            mentions: vec![TeamBoardMention {
                identity: TeamBoardIdentity::Agent { address },
                display: "@Reviewer".into(),
                start: 0,
                end: 9,
            }],
        },
    });
    let error = coordinator
        .execute_operation(operation.clone())
        .await
        .unwrap_err();
    let rejected = error
        .downcast_ref::<TeamOperationRejected>()
        .context("definitive rejection")?
        .clone();
    assert_eq!(rejected.reason, "board agent is not enrolled");

    // Restoring the originally selected identity makes a new submission valid,
    // but must never change the terminal outcome of an existing operation ID.
    coordinator.store.put("agent", &key, &enrolled).await?;
    let (first, second) = tokio::join!(
        coordinator.execute_operation(operation.clone()),
        coordinator.execute_operation(operation.clone()),
    );
    for result in [first, second] {
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<TeamOperationRejected>(),
            Some(&rejected)
        );
    }
    assert!(coordinator.board_snapshot().await?.entries.is_empty());
    drop(coordinator);

    let coordinator = Coordinator::open(manifest).await?;
    let error = coordinator
        .execute_operation(operation.clone())
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<TeamOperationRejected>(),
        Some(&rejected)
    );
    let fresh = TeamOperation::new(operation.request.clone());
    coordinator.execute_operation(fresh.clone()).await?;
    let entries = coordinator.board_snapshot().await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].message.id, fresh.operation_id);
    assert_ne!(entries[0].message.id, operation.operation_id);
    assert_eq!(
        coordinator
            .store
            .get::<TeamOperationRejected>("operator_rejection", &operation.operation_id)
            .await?,
        Some(rejected)
    );
    Ok(())
}
