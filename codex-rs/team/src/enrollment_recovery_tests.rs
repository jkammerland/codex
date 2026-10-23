use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use anyhow::Result;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_team_store::TeamStore;
use codex_utils_absolute_path::AbsolutePathBuf;
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
use crate::model::TeamRequest;

const THREAD_ID: &str = "00000000-0000-0000-0000-000000000042";

#[tokio::test]
async fn enrollment_recovery_replays_original_params_after_restart_and_newer_control() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let worker_directory = tempfile::tempdir()?;
    let sqlite =
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(worker_directory.path())?);
    let worker = TeamStore::open(&sqlite, &worker_directory.path().join("worker.sqlite")).await?;
    let server_worker = worker.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut requests = Vec::new();
        let mut read_count = 0;
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
            let result: Result<Value> = async {
                match request["method"].as_str().context("missing method")? {
                    "initialize" => Ok(json!({"userAgent":"fixture/1"})),
                    "thread/loaded/list" => Ok(json!({"data":[THREAD_ID],"nextCursor":null})),
                    "team/enroll" => {
                        let params: TeamEnrollParams = serde_json::from_value(request["params"].clone())?;
                        requests.push(params.clone());

                        // Use the worker's real durable operation ledger and exact wire digest.
                        let operation_id = format!("{}:{}", params.control.coordinator_id,
                            params.operation_id.as_ref().context("enrollment operation is required")?);
                        let digest = serde_json::to_string(&params)?;
                        let enrollment = match server_worker.host_operation_result::<TeamEnrollment>(&operation_id, &digest).await? {
                            Some(enrollment) => enrollment,
                            None => {
                                server_worker.apply_control(&params.control).await?;
                                server_worker.enroll_operation(&TeamEnrollment {
                                    address: params.address,
                                    alias: params.alias,
                                    manually_held: false,
                                    continuation_turn_id: None,
                                }, "fixture-credential", Some((&operation_id, &digest))).await?
                            }
                        };
                        Ok(json!({"enrollment":enrollment}))
                    }
                    "thread/read" => {
                        read_count += 1;
                        if read_count == 1 {
                            anyhow::bail!("injected metadata failure after durable enrollment");
                        }
                        Ok(json!({"thread":{"status":{"type":"idle"},"projectId":null}}))
                    }
                    "team/control" => {
                        let params: TeamControlParams = serde_json::from_value(request["params"].clone())?;
                        Ok(json!({"control":server_worker.apply_control(&params.control).await?,"reconciled":true}))
                    }
                    method => anyhow::bail!("unexpected enrollment recovery request {method}"),
                }
            }.await;
            let response = match result {
                Ok(result) => json!({"id":id,"result":result}),
                Err(error) => json!({"id":id,"error":{"code":-32603,"message":error.to_string()}}),
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await?;
        }
        Ok::<_, anyhow::Error>((requests, read_count))
    });

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
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server],
    }];
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint,
            auth_token: None,
        },
        client_name: "enrollment-recovery-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await?;
    let peer = Peer {
        handle: AppServerRequestHandle::Remote(client.request_handle()),
        verified: Arc::new(AtomicBool::new(true)),
    };
    let address = TeamAddress {
        machine_id: "worker".into(),
        server_id: "primary".into(),
        thread_id: THREAD_ID.into(),
    };
    let operation_id = uuid::Uuid::new_v4().to_string();

    let result = async {
        let coordinator = Coordinator::open(manifest.clone()).await?;
        coordinator
            .peers
            .write()
            .await
            .insert("worker/primary".into(), peer.clone());
        let original_control = coordinator.current_control().await?;
        let original = TeamEnrollParams {
            operation_id: Some(operation_id.clone()),
            address: address.clone(),
            alias: "Builder".into(),
            control: original_control,
        };
        let failure = coordinator
            .enroll(address.clone(), "Builder".into(), Some(&operation_id))
            .await
            .expect_err("metadata failure must leave coordinator enrollment pending");
        assert!(failure.to_string().contains("injected metadata failure"));
        assert!(coordinator.agents().await?.is_empty());
        assert_eq!(worker.enrollments("", 1000).await?.len(), 1);
        assert_eq!(
            coordinator
                .store
                .get::<TeamEnrollParams>("enrollment_request", &operation_id)
                .await?,
            Some(original.clone())
        );

        // Pause travels independently while the original enrollment has an incomplete acknowledgment.
        coordinator
            .execute(TeamRequest::Control {
                mode: TeamExecutionMode::Paused,
            })
            .await?;
        let newer_control = coordinator.current_control().await?;
        let _: TeamControlResponse = peer
            .handle
            .request_typed(ClientRequest::TeamControl {
                request_id: RequestId::String("newer-control".into()),
                params: TeamControlParams {
                    control: newer_control.clone(),
                },
            })
            .await?;
        drop(coordinator);

        let restored = Coordinator::open(manifest).await?;
        restored
            .peers
            .write()
            .await
            .insert("worker/primary".into(), peer.clone());
        let recovered = restored
            .enroll(address.clone(), "Builder".into(), Some(&operation_id))
            .await?;
        assert_eq!(recovered.enrollment.address, address);
        assert_eq!(recovered.enrollment.alias, "Builder");
        assert_eq!(restored.agents().await?.len(), 1);
        assert_eq!(
            worker.enrollments("", 1000).await?,
            vec![recovered.enrollment]
        );
        assert_eq!(worker.control().await?, Some(newer_control.clone()));
        assert_eq!(restored.current_control().await?, newer_control);
        Ok::<_, anyhow::Error>(original)
    }
    .await;

    client.shutdown().await?;
    let (requests, read_count) = server_task.await??;
    let original = result?;
    assert_eq!(requests, vec![original.clone(), original]);
    assert_eq!(read_count, 2);
    Ok(())
}

#[tokio::test]
async fn missing_enrollment_session_names_its_selected_host_without_misclassifying_other_failures()
-> Result<()> {
    #[derive(Clone, Copy)]
    enum ResumeFailure {
        Missing,
        Denied,
        Disconnected,
        InvalidResponse,
    }

    for failure in [
        ResumeFailure::Missing,
        ResumeFailure::Denied,
        ResumeFailure::Disconnected,
        ResumeFailure::InvalidResponse,
    ] {
        let directory = tempfile::tempdir()?;
        let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("ws://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut socket = tokio_tungstenite::accept_async(stream).await?;
            let mut requests = Vec::new();
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
                let method = request["method"].as_str().context("request method")?;
                requests.push(method.to_owned());
                let response = match method {
                    "initialize" => json!({"id": id, "result": {
                        "userAgent": "fixture/1", "codexHome": "/fixture",
                        "platformFamily": "unix", "platformOs": "linux"
                    }}),
                    "thread/loaded/list" => json!({"id": id, "result": {
                        "data": [], "nextCursor": null
                    }}),
                    "thread/resume" => match failure {
                        ResumeFailure::Missing => json!({"id": id, "error": {
                            "code": -32600,
                            "message": format!("no rollout found for thread id {THREAD_ID}")
                        }}),
                        ResumeFailure::Denied => json!({"id": id, "error": {
                            "code": -32600, "message": "session access denied"
                        }}),
                        ResumeFailure::Disconnected => {
                            socket.close(None).await?;
                            break;
                        }
                        ResumeFailure::InvalidResponse => {
                            socket.send(Message::Text("{".into())).await?;
                            break;
                        }
                    },
                    method => anyhow::bail!("unexpected enrollment request: {method}"),
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await?;
            }
            Ok::<_, anyhow::Error>(requests)
        });
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: endpoint,
                auth_token: None,
            },
            client_name: "enrollment-diagnostic-test".into(),
            client_version: "test".into(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 16,
        })
        .await?;
        coordinator.peers.write().await.insert(
            "local/primary".into(),
            Peer {
                handle: AppServerRequestHandle::Remote(client.request_handle()),
                verified: Arc::new(AtomicBool::new(true)),
            },
        );
        let error = coordinator
            .enroll(
                TeamAddress {
                    machine_id: "local".into(),
                    server_id: "primary".into(),
                    thread_id: THREAD_ID.into(),
                },
                "Reviewer".into(),
                /*operation_id*/ None,
            )
            .await
            .unwrap_err();
        client.shutdown().await?;

        // No failed resume may continue to team/enroll or another host.
        assert_eq!(
            server.await??,
            ["initialize", "thread/loaded/list", "thread/resume"]
        );
        assert!(coordinator.agents().await?.is_empty());
        let typed = error
            .downcast_ref::<TypedRequestError>()
            .context("original resume error remains available")?;
        match failure {
            ResumeFailure::Missing => {
                assert_eq!(
                    error.to_string(),
                    format!(
                        "Session {THREAD_ID} was not found on selected server local/primary. Enrollment requires an existing session on that server. In /agents, choose the session from its owning host and use Enroll existing session, or retry with that host's machine/server/thread-id address."
                    )
                );
                assert!(matches!(typed, TypedRequestError::Server { .. }));
            }
            ResumeFailure::Denied => {
                assert_eq!(
                    error.to_string(),
                    "thread/resume failed: session access denied (code -32600)"
                );
            }
            ResumeFailure::Disconnected | ResumeFailure::InvalidResponse => {
                assert!(matches!(typed, TypedRequestError::Transport { .. }));
                assert_eq!(error.to_string(), typed.to_string());
            }
        }
    }
    Ok(())
}
