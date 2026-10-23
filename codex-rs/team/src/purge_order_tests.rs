use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Result;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::*;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::model::TeamOperation;
use crate::model::TeamRequest;

#[derive(Debug, PartialEq, Eq)]
enum Trace {
    Sync {
        purges: Vec<String>,
        verified: bool,
    },
    Control {
        revision: i64,
        mode: TeamExecutionMode,
    },
    Outbox,
}

#[derive(Clone, Copy)]
enum Acknowledgements {
    Complete,
    Missing,
    Partial,
    Extra,
}

struct Outcome {
    result: Result<()>,
    trace: Vec<Trace>,
    verified: bool,
    before: Vec<TeamBoardPurgeOperation>,
    after: Vec<TeamBoardPurgeOperation>,
}

async fn reconnect(acknowledgements: Acknowledgements) -> Result<Outcome> {
    let home = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = Server {
        id: "primary".into(),
        label: "Primary".into(),
        endpoint: crate::Endpoint::Websocket {
            url: endpoint.clone(),
            token_file: None,
        },
        cancellation_adapters: Vec::new(),
        environment_capabilities: Default::default(),
    };
    let machine = Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    };
    let mut manifest = crate::tests::manifest(home.path());
    manifest.machines = vec![machine.clone()];
    let coordinator = Coordinator::open(manifest).await?;
    let original_control = coordinator.current_control().await?;
    coordinator
        .store
        .put("installation", "worker/primary", &"installation")
        .await?;

    // The disconnected worker still has Running revision zero. Reconnect must
    // erase its cached Board work before releasing either recorded control.
    for mode in [TeamExecutionMode::Stopped, TeamExecutionMode::Running] {
        coordinator.execute(TeamRequest::Control { mode }).await?;
    }
    let mut before = Vec::new();
    for topic in ["first", "second"] {
        coordinator
            .execute(TeamRequest::Post {
                topic: topic.into(),
                body: "Remove this cached work".into(),
            })
            .await?;
        let purge_id = uuid::Uuid::new_v4().to_string();
        coordinator
            .execute_operation(TeamOperation {
                operation_id: purge_id.clone(),
                request: TeamRequest::BoardPurge {
                    scope: TeamBoardPurgeScope::All,
                    expected_revision: coordinator.store.board_clock().await?.revision,
                },
            })
            .await?;
        let pending = coordinator
            .store
            .board_purge_operation(&purge_id)
            .await?
            .unwrap();
        assert_eq!(pending.workers.len(), 1);
        assert!(!pending.workers[0].acknowledged);
        before.push(pending);
    }

    let verified = Arc::new(AtomicBool::new(false));
    let server_verified = verified.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut control = original_control;
        let mut trace = Vec::new();
        while let Some(message) = socket.next().await {
            let message = message?;
            if matches!(message, Message::Close(_)) {
                break;
            }
            let Message::Text(text) = message else {
                continue;
            };
            let request: Value = serde_json::from_str(&text)?;
            let Some(id) = request.get("id") else {
                continue;
            };
            let result = match request["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"userAgent":"fixture/1","codexHome":"/fixture","platformFamily":"unix","platformOs":"linux"})
                }
                "team/hostRead" => json!({
                    "installationId":"installation","protocolVersion":6,
                    "control":control,"data":[],"runtime":[],"nextCursor":null,
                }),
                "team/sync" => {
                    let params: TeamSyncParams = serde_json::from_value(request["params"].clone())?;
                    let purges = params
                        .board_purges
                        .into_iter()
                        .map(|journal| journal.operation_id)
                        .collect::<Vec<_>>();
                    trace.push(Trace::Sync {
                        purges: purges.clone(),
                        verified: server_verified.load(Ordering::Acquire),
                    });
                    let returned = match acknowledgements {
                        Acknowledgements::Complete => purges,
                        Acknowledgements::Missing => Vec::new(),
                        Acknowledgements::Partial => purges.into_iter().take(1).collect(),
                        Acknowledgements::Extra => {
                            let mut returned = purges;
                            returned.push("unsent-purge".into());
                            returned
                        }
                    };
                    serde_json::to_value(TeamSyncResponse {
                        board_purge_acknowledgements: returned,
                    })?
                }
                "team/control" => {
                    let params: TeamControlParams =
                        serde_json::from_value(request["params"].clone())?;
                    control = params.control;
                    trace.push(Trace::Control {
                        revision: control.revision,
                        mode: control.mode,
                    });
                    serde_json::to_value(TeamControlResponse {
                        control: control.clone(),
                        reconciled: true,
                    })?
                }
                "team/outboxList" => {
                    trace.push(Trace::Outbox);
                    json!({"data":[],"nextCursor":null})
                }
                method => anyhow::bail!("unexpected purge-order request {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .await?;
        }
        Ok::<_, anyhow::Error>(trace)
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint,
            auth_token: None,
        },
        client_name: "purge-order-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await?;
    let peer = Peer {
        handle: AppServerRequestHandle::Remote(client.request_handle()),
        verified,
    };
    let result = async {
        coordinator.reconcile(&machine, &server, &peer).await?;
        coordinator.synchronize(&machine, &server, &peer).await
    }
    .await;
    client.shutdown().await?;
    let trace = server_task.await??;
    let mut after = Vec::new();
    for operation in &before {
        after.push(
            coordinator
                .store
                .board_purge_operation(&operation.operation_id)
                .await?
                .unwrap(),
        );
    }
    Ok(Outcome {
        result,
        trace,
        verified: peer.verified.load(Ordering::Acquire),
        before,
        after,
    })
}

#[tokio::test]
async fn reconnect_purges_cached_mail_before_verification_and_stop_resume_replay() -> Result<()> {
    let outcome = reconnect(Acknowledgements::Complete).await?;
    outcome.result?;
    let ids = outcome
        .before
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect();
    assert_eq!(
        outcome.trace,
        vec![
            Trace::Sync {
                purges: ids,
                verified: false
            },
            Trace::Control {
                revision: 1,
                mode: TeamExecutionMode::Stopped
            },
            Trace::Control {
                revision: 2,
                mode: TeamExecutionMode::Running
            },
            Trace::Outbox,
            Trace::Sync {
                purges: Vec::new(),
                verified: true
            },
            Trace::Outbox,
            Trace::Sync {
                purges: Vec::new(),
                verified: true
            },
        ]
    );
    assert!(outcome.verified);
    for (before, after) in outcome.before.iter().zip(outcome.after) {
        assert!(after.workers[0].acknowledged);
        assert_eq!(after.revision, before.revision);
    }
    Ok(())
}

fn assert_reconnect_blocked(outcome: Outcome) {
    assert!(
        outcome
            .result
            .expect_err("incomplete or extra acknowledgments cannot release controls")
            .to_string()
            .contains("complete Board purge batch")
    );
    let ids = outcome
        .before
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect();
    assert_eq!(
        outcome.trace,
        vec![Trace::Sync {
            purges: ids,
            verified: false
        }]
    );
    assert!(!outcome.verified);
    assert_eq!(outcome.after, outcome.before);
}

#[tokio::test]
async fn missing_purge_acknowledgment_keeps_reconnect_unverified_and_controls_held() -> Result<()> {
    assert_reconnect_blocked(reconnect(Acknowledgements::Missing).await?);
    Ok(())
}

#[tokio::test]
async fn partial_purge_acknowledgment_does_not_commit_partial_progress_or_resume() -> Result<()> {
    assert_reconnect_blocked(reconnect(Acknowledgements::Partial).await?);
    Ok(())
}

#[tokio::test]
async fn extra_purge_acknowledgment_does_not_commit_progress_or_resume() -> Result<()> {
    assert_reconnect_blocked(reconnect(Acknowledgements::Extra).await?);
    Ok(())
}
