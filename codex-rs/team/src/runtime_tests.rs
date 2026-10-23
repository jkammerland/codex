//! Exercises real app-server processes, authenticated listeners and the relay.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;

use crate::config::Endpoint;
use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::request_id;
use crate::model::*;

async fn dual_host(home: &Path) -> Result<(Child, Server)> {
    dual_host_on(home, "127.0.0.1:0").await
}

async fn dual_host_on(home: &Path, listen: &str) -> Result<(Child, Server)> {
    // Let the listener create its private socket directory, including the
    // protected user-only ACL required by the Windows socket transport.
    let socket = home.join("socket/daemon.sock");
    let token = home.join("worker-token");
    tokio::fs::write(&token, "test-worker-token").await?;

    // Keep the readiness URL plain despite inherited color overrides.
    let mut process = Command::new(codex_utils_cargo_bin::cargo_bin("codex-app-server")?)
        .args([
            "--listen",
            &format!("unix://{}", socket.display()),
            "--listen-loopback",
            listen,
            "--ws-auth",
            "capability-token",
            "--ws-token-file",
        ])
        .arg(&token)
        .arg(app_test_support::DISABLE_PLUGIN_STARTUP_TASKS_ARG)
        .current_dir(home)
        .env("CODEX_HOME", home)
        .env("RUST_LOG", "warn")
        .env("NO_COLOR", "1")
        .env_remove("FORCE_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("IGNORE_IS_TERMINAL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(process.stderr.take().context("missing stderr")?).lines();
    let address = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(address) = line
                .split_whitespace()
                .find_map(|word| word.strip_prefix("ws://"))
                .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
            {
                return Ok::<_, anyhow::Error>(address);
            }
            eprintln!("{line}");
        }
        anyhow::bail!("host exited before reporting its listener")
    })
    .await??;
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok((
        process,
        Server {
            environment_capabilities: Default::default(),
            id: "primary".into(),
            label: "Primary".into(),
            cancellation_adapters: Vec::new(),
            endpoint: Endpoint::Websocket {
                url: format!("ws://{address}"),
                token_file: Some(token),
            },
        },
    ))
}

async fn client(endpoint: RemoteAppServerEndpoint) -> Result<RemoteAppServerClient> {
    Ok(RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint,
        client_name: "team-runtime-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 256,
    })
    .await?)
}

async fn until(
    coordinator: &Arc<Coordinator>,
    phase: &str,
    predicate: impl Fn(&TeamSnapshot) -> bool,
) -> Result<TeamSnapshot> {
    eprintln!("waiting for {phase}");
    let mut changed = coordinator.changed.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            let snapshot = coordinator.snapshot().await?;
            if predicate(&snapshot) {
                eprintln!("reached {phase}");
                return Ok(snapshot);
            }
            changed.changed().await?;
        }
    })
    .await
    .context("timed out waiting for coordinator state")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hosts_exchange_mail_through_relay_and_replay_offline_controls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home_a = tempfile::tempdir()?;
    let home_b = tempfile::tempdir()?;
    let model =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "handled",
        )?])
        .await;
    let result: Result<()> = async {
        MockResponsesConfig::new(&model.uri()).write(home_a.path())?;
        MockResponsesConfig::new(&model.uri()).write(home_b.path())?;
        let (mut process_a, server_a) = dual_host(home_a.path()).await?;
        let (mut process_b, server_b) = dual_host(home_b.path()).await?;
        let mut manifest = crate::tests::manifest(directory.path());
        manifest.machines = vec![
            Machine {
                id: "a".into(),
                label: "Machine A".into(),
                servers: vec![server_a.clone()],
            },
            Machine {
                id: "b".into(),
                label: "Machine B".into(),
                servers: vec![server_b.clone()],
            },
        ];
        let coordinator = Coordinator::open(manifest.clone()).await?;
        let mut workers = crate::worker::start(&coordinator);
        until(&coordinator, "initial worker connections", |state| {
            state.servers.iter().all(|server| server.connected)
        })
        .await?;

        // The socket and the authenticated listener reach the same daemon state.
        let local = client(RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::try_from(home_a.path().join("socket/daemon.sock"))?,
        })
        .await?;
        let started_a: ThreadStartResponse = local
            .request_typed(ClientRequest::ThreadStart {
                request_id: request_id(),
                params: ThreadStartParams::default(),
            })
            .await?;
        let peer_b = coordinator.peer("b", "primary").await?;
        let started_b: ThreadStartResponse = peer_b
            .handle
            .request_typed(ClientRequest::ThreadStart {
                request_id: request_id(),
                params: ThreadStartParams::default(),
            })
            .await?;
        let address_a = TeamAddress {
            machine_id: "a".into(),
            server_id: "primary".into(),
            thread_id: started_a.thread.id,
        };
        let address_b = TeamAddress {
            machine_id: "b".into(),
            server_id: "primary".into(),
            thread_id: started_b.thread.id,
        };
        for (address, alias) in [
            (address_a.clone(), "author"),
            (address_b.clone(), "reviewer"),
        ] {
            coordinator
                .execute(TeamRequest::Enroll {
                    address,
                    alias: alias.into(),
                })
                .await?;
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let bind = listener.local_addr()?;
        let app = crate::http::router(coordinator.clone(), "operator-token".into());
        let http = tokio::spawn(async move { axum::serve(listener, app).await });
        let relay_url = format!("ws://{bind}/relay/a/primary");
        assert!(
            client(RemoteAppServerEndpoint::WebSocket {
                websocket_url: relay_url.clone(),
                auth_token: None
            })
            .await
            .is_err()
        );
        let relay = client(RemoteAppServerEndpoint::WebSocket {
            websocket_url: relay_url,
            auth_token: Some("operator-token".into()),
        })
        .await?;
        let operator = crate::TeamClient::new(&format!("http://{bind}"), "operator-token".into())?;
        assert!(
            crate::TeamClient::new(&format!("http://{bind}"), "wrong-token".into())?
                .snapshot()
                .await
                .is_err()
        );

        // Equal request IDs on independent connections must not cross responses.
        let peer_a = coordinator.peer("a", "primary").await?;
        let same_id = RequestId::Integer(77);
        let (read, models) = tokio::try_join!(
            relay.request_typed::<ThreadReadResponse>(ClientRequest::ThreadRead {
                request_id: same_id.clone(),
                params: ThreadReadParams {
                    thread_id: address_a.thread_id.clone(),
                    include_turns: false
                }
            }),
            peer_a
                .handle
                .request_typed::<ModelListResponse>(ClientRequest::ModelList {
                    request_id: same_id,
                    params: ModelListParams {
                        cursor: None,
                        limit: Some(10),
                        include_hidden: Some(true)
                    }
                })
        )?;
        assert_eq!(read.thread.id, address_a.thread_id);
        assert!(!models.data.is_empty());
        relay.shutdown().await?;

        operator
            .request(TeamRequest::Control {
                mode: TeamExecutionMode::Paused,
            })
            .await?;
        until(&coordinator, "Pause acknowledgements", |state| {
            state
                .servers
                .iter()
                .all(|server| server.applied_revision == Some(state.control.revision))
        })
        .await?;
        let credential: TeamAgentCredentialResponse = local
            .request_typed(ClientRequest::TeamAgentCredential {
                request_id: request_id(),
                params: TeamAgentCredentialParams {
                    thread_id: address_a.thread_id.clone(),
                },
            })
            .await?;
        // A bad recipient is retained as a rejected envelope, and cannot
        // strand the next valid message in this host's durable outbox.
        let _: TeamAgentRequestResponse = local
            .request_typed(ClientRequest::TeamAgentRequest {
                request_id: request_id(),
                params: TeamAgentRequestParams {
                    operation_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: address_a.thread_id.clone(),
                    credential: credential.credential.clone(),
                    action: TeamAgentActionParams::Send {
                        recipient: TeamAddress {
                            thread_id: uuid::Uuid::new_v4().to_string(),
                            ..address_b.clone()
                        }
                        .into(),
                        body: "Invalid recipient".into(),
                        kind: TeamMessageKind::Request,
                        reply_to: None,
                        delivery: TeamDeliveryMode::NextTurn,
                    },
                },
            })
            .await?;
        let _: TeamAgentRequestResponse = local
            .request_typed(ClientRequest::TeamAgentRequest {
                request_id: request_id(),
                params: TeamAgentRequestParams {
                    operation_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: address_a.thread_id.clone(),
                    credential: credential.credential,
                    action: TeamAgentActionParams::Send {
                        recipient: address_b.clone().into(),
                        body: "Please review".into(),
                        kind: TeamMessageKind::Request,
                        reply_to: None,
                        delivery: TeamDeliveryMode::NextTurn,
                    },
                },
            })
            .await?;
        let pending = until(&coordinator, "outbox ingestion", |state| {
            state.messages.len() == 1
        })
        .await?;
        assert!(!pending.messages[0].published);
        assert_eq!(pending.rejected_outbox.len(), 1);
        assert!(
            pending.rejected_outbox[0]
                .reason
                .contains("recipient is not enrolled")
        );
        assert!(
            model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.url.path().ends_with("/responses"))
        );
        operator
            .request(TeamRequest::Control {
                mode: TeamExecutionMode::Running,
            })
            .await?;
        let completed = until(&coordinator, "delivery completion", |state| {
            state.messages[0]
                .receipts
                .values()
                .any(|receipt| receipt.state == TeamReceiptState::Completed)
        })
        .await?;
        assert_eq!(completed.messages[0].message.sender, address_a);
        assert_eq!(completed.messages[0].targets, Some(vec![address_b.clone()]));
        assert_eq!(
            model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path().ends_with("/responses"))
                .count(),
            1
        );

        // An archived session is a user hold, not an idle target. Reconnect
        // must preserve it even when a new addressed request is queued.
        let _: ThreadArchiveResponse = peer_b
            .handle
            .request_typed(ClientRequest::ThreadArchive {
                request_id: request_id(),
                params: ThreadArchiveParams {
                    thread_id: address_b.thread_id.clone(),
                },
            })
            .await?;
        until(&coordinator, "archived member", |state| {
            state
                .agents
                .iter()
                .any(|agent| agent.enrollment.address == address_b && agent.status == "archived")
        })
        .await?;
        coordinator
            .execute(TeamRequest::Send {
                recipient: address_b.clone(),
                body: "Hold until unarchived".into(),
                kind: TeamMessageKind::Request,
                reply_to: None,
                delivery: TeamDeliveryMode::NextTurn,
            })
            .await?;

        // Reconnect must replay Stop even when Resume is already the latest state.
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        coordinator
            .execute(TeamRequest::Control {
                mode: TeamExecutionMode::Stopped,
            })
            .await?;
        coordinator
            .execute(TeamRequest::Control {
                mode: TeamExecutionMode::Running,
            })
            .await?;
        // A host restart restores the gate before any session can execute.
        local.shutdown().await?;
        process_a.kill().await?;
        let Endpoint::Websocket { url, .. } = &server_a.endpoint else {
            unreachable!()
        };
        let (restarted, _) =
            dual_host_on(home_a.path(), url.strip_prefix("ws://").unwrap()).await?;
        process_a = restarted;
        let local = client(RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::try_from(home_a.path().join("socket/daemon.sock"))?,
        })
        .await?;
        let _: ThreadResumeResponse = local
            .request_typed(ClientRequest::ThreadResume {
                request_id: request_id(),
                params: ThreadResumeParams {
                    thread_id: address_a.thread_id.clone(),
                    ..Default::default()
                },
            })
            .await?;
        let restored: TeamHostReadResponse = local
            .request_typed(ClientRequest::TeamHostRead {
                request_id: request_id(),
                params: TeamHostReadParams {
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        assert!(restored.runtime[0].reconciling);
        assert!(
            local
                .request_typed::<TurnStartResponse>(ClientRequest::TurnStart {
                    request_id: request_id(),
                    params: TurnStartParams {
                        thread_id: address_a.thread_id.clone(),
                        input: vec![UserInput::Text {
                            text: "Cannot execute before reconciliation".into(),
                            text_elements: Vec::new()
                        }],
                        ..Default::default()
                    },
                })
                .await
                .is_err()
        );
        let _workers = crate::worker::start(&coordinator);
        until(&coordinator, "reconnect acknowledgements", |state| {
            state
                .servers
                .iter()
                .all(|server| server.applied_revision == Some(state.control.revision))
        })
        .await?;
        let host: TeamHostReadResponse = local
            .request_typed(ClientRequest::TeamHostRead {
                request_id: request_id(),
                params: TeamHostReadParams {
                    cursor: None,
                    limit: None,
                },
            })
            .await?;
        assert_eq!(host.control, Some(coordinator.current_control().await?));
        assert_eq!(host.data.len(), 1);
        let snapshot = coordinator.snapshot().await?;
        assert_eq!(
            snapshot
                .agents
                .iter()
                .find(|agent| agent.enrollment.address == address_b)
                .unwrap()
                .status,
            "archived"
        );
        assert_eq!(
            model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path().ends_with("/responses"))
                .count(),
            1
        );
        local.shutdown().await?;
        http.abort();
        process_a.kill().await?;
        process_b.kill().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        eprintln!("runtime scenario failed: {error:#}");
    }
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_finishes_persisted_stop_before_a_later_resume() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let mut manifest = crate::tests::manifest(directory.path());
    let coordinator = Coordinator::open(manifest.clone()).await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        })
        .await?;
    let stopped = coordinator.current_control().await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        })
        .await?;
    drop(coordinator);

    MockResponsesConfig::new("http://127.0.0.1:1")
        .with_root_config(&format!(
            "sqlite_home = {}",
            serde_json::to_string(&home.path().display().to_string())?
        ))
        .write(home.path())?;
    let sqlite =
        codex_state::SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let host_store =
        codex_team_store::TeamStore::open(&sqlite, &home.path().join("team/host.sqlite")).await?;
    let root = app_test_support::create_fake_rollout(
        home.path(),
        "2026-10-01T12-00-00",
        "2026-10-01T12:00:00Z",
        "Saved ordinary task",
        Some("mock_provider"),
        None,
    )?;
    host_store
        .enroll(
            &TeamEnrollment {
                address: TeamAddress {
                    machine_id: "worker".into(),
                    server_id: "primary".into(),
                    thread_id: root.clone(),
                },
                alias: "saved-root".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            "saved-credential",
        )
        .await?;

    // Simulate a crash after persisting Stop but before its cancellation intent
    // was recorded for an enrolled task. Unrelated external jobs stay outside
    // the team scope; this launch has no handle so cancellation stays pending.
    host_store.apply_control(&stopped).await?;
    host_store
        .put(
            "job",
            "pending-launch",
            &serde_json::json!({
                "job": TeamTrackedJob {
                    id: "pending-launch".into(),
                    thread_id: root.clone(),
                    server: "jobs".into(),
                    tool: "launch".into(),
                    handle: None,
                    status: TeamJobStatus::Unconfirmed,
                    detail: Some("Launch handle is not known yet".into()),
                },
                "adapter": null,
                "requested_revision": null,
                "attempted_revision": null,
                "registration_fingerprint": null,
            }),
        )
        .await?;
    let (mut process, server) = dual_host(home.path()).await?;
    manifest.machines = vec![Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server],
    }];
    let coordinator = Coordinator::open(manifest).await?;
    let _workers = crate::worker::start(&coordinator);
    until(&coordinator, "recovered Stop then Resume", |snapshot| {
        snapshot.servers[0].connected
            && snapshot.servers[0].applied_revision == Some(2)
            && snapshot.agents.iter().any(|agent| {
                agent.enrollment.address.thread_id == root
                    && agent
                        .runtime
                        .as_ref()
                        .is_some_and(|runtime| !runtime.reconciling)
            })
    })
    .await?;
    let job = host_store
        .get::<serde_json::Value>("job", "pending-launch")
        .await?
        .context("pending launch must remain durable")?;
    assert_eq!(job["requested_revision"], serde_json::json!(1));
    assert_eq!(host_store.control().await?.unwrap().revision, 2);
    process.kill().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_root_browsing_uses_relay_before_team_reconciliation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    // Empty new threads are persisted lazily. Browse existing ordinary history
    // so listing has durable input without issuing an inference request.
    let root_id = app_test_support::create_fake_rollout(
        home.path(),
        "2026-10-01T12-00-00",
        "2026-10-01T12:00:00Z",
        "Saved ordinary task",
        Some("openai"),
        None,
    )?;
    let (mut process, server) = dual_host(home.path()).await?;
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![Machine {
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    }];
    let coordinator = Coordinator::open(manifest).await?;
    let endpoint = crate::transport::ResolvedEndpoint::open(&server.endpoint).await?;
    coordinator
        .endpoints
        .write()
        .await
        .insert("worker/primary".into(), endpoint);
    assert!(!coordinator.snapshot().await?.servers[0].connected);
    assert!(coordinator.peer("worker", "primary").await.is_err());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let bind = listener.local_addr()?;
    let app = crate::http::router(coordinator.clone(), "operator-token".into());
    let http = tokio::spawn(async move { axum::serve(listener, app).await });
    let relay = client(RemoteAppServerEndpoint::WebSocket {
        websocket_url: format!("ws://{bind}/relay/worker/primary"),
        auth_token: Some("operator-token".into()),
    })
    .await?;
    let root: ThreadResumeResponse = relay
        .request_typed(ClientRequest::ThreadResume {
            request_id: request_id(),
            params: ThreadResumeParams {
                thread_id: root_id.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(root.thread.id, root_id);
    let list: ThreadListResponse = relay
        .request_typed(ClientRequest::ThreadList {
            request_id: request_id(),
            params: serde_json::from_value(serde_json::json!({}))?,
        })
        .await?;
    assert!(list.data.iter().any(|thread| thread.id == root.thread.id));
    assert!(coordinator.agents().await?.is_empty());
    relay.shutdown().await?;
    http.abort();
    process.kill().await?;
    process.wait().await?;
    Ok(())
}

#[tokio::test]
async fn blocked_enrollment_on_one_host_does_not_block_another_host() -> Result<()> {
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;

    let directory = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let (mut process, server) = dual_host(home.path()).await?;
    let local = client(RemoteAppServerEndpoint::UnixSocket {
        socket_path: AbsolutePathBuf::try_from(home.path().join("socket/daemon.sock"))?,
    })
    .await?;
    let root: ThreadStartResponse = local
        .request_typed(ClientRequest::ThreadStart {
            request_id: request_id(),
            params: ThreadStartParams::default(),
        })
        .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let fake_url = format!("ws://{}", listener.local_addr()?);
    let (admitted, blocked) = tokio::sync::oneshot::channel();
    let fake_server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        while let Some(message) = socket.next().await {
            let message = message?;
            let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text)?;
            if request["method"] == "initialize" {
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        json!({"id":request["id"],"result":{"userAgent":"fixture/1"}})
                            .to_string()
                            .into(),
                    ))
                    .await?;
            } else if request["method"] == "thread/loaded/list" {
                let _ = admitted.send(());
                // This host deliberately never acknowledges the request. The
                // second enrollment must finish while its original RPC waits.
                std::future::pending::<()>().await;
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    let fake_client = client(RemoteAppServerEndpoint::WebSocket {
        websocket_url: fake_url,
        auth_token: None,
    })
    .await?;
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![Machine {
        id: "fast".into(),
        label: "Fast".into(),
        servers: vec![server],
    }];
    let coordinator = Coordinator::open(manifest).await?;
    coordinator.peers.write().await.insert(
        "slow/primary".into(),
        crate::coordinator::Peer {
            handle: codex_app_server_client::AppServerRequestHandle::Remote(
                fake_client.request_handle(),
            ),
            verified: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        },
    );
    coordinator.peers.write().await.insert(
        "fast/primary".into(),
        crate::coordinator::Peer {
            handle: codex_app_server_client::AppServerRequestHandle::Remote(local.request_handle()),
            verified: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        },
    );
    let slow_coordinator = coordinator.clone();
    let slow_enrollment = tokio::spawn(async move {
        slow_coordinator
            .execute(TeamRequest::Enroll {
                address: TeamAddress {
                    machine_id: "slow".into(),
                    server_id: "primary".into(),
                    thread_id: uuid::Uuid::new_v4().to_string(),
                },
                alias: "slow".into(),
            })
            .await
    });
    blocked.await?;
    coordinator
        .execute(TeamRequest::Control {
            mode: TeamExecutionMode::Paused,
        })
        .await?;
    let response = coordinator
        .execute(TeamRequest::Enroll {
            address: TeamAddress {
                machine_id: "fast".into(),
                server_id: "primary".into(),
                thread_id: root.thread.id,
            },
            alias: "fast".into(),
        })
        .await?;
    assert!(matches!(response, TeamResponse::Agent { .. }));
    assert!(!slow_enrollment.is_finished());
    slow_enrollment.abort();
    let _ = slow_enrollment.await;
    fake_client.shutdown().await?;
    fake_server.abort();
    local.shutdown().await?;
    process.kill().await?;
    process.wait().await?;
    Ok(())
}
