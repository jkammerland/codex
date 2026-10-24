use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use super::*;

const THREAD_ID: &str = "00000000-0000-7000-8000-000000000001";

// The response shape comes from a real Windows worker. Account paths, IDs and
// conversation text are replaced; nested Windows paths remain host-owned text.
fn windows_thread_read() -> Value {
    json!({
        "thread": {
            "id": THREAD_ID,
            "environments": [{
                "environmentId": "local",
                "cwd": r"C:\fleet\workspace",
                "runtimeWorkspaceRoots": [r"C:\fleet\workspace"]
            }],
            "extra": null,
            "sessionId": THREAD_ID,
            "forkedFromId": null,
            "parentThreadId": null,
            "preview": "Saved Windows conversation",
            "ephemeral": false,
            "section": null,
            "sectionEnteredAt": null,
            "projectId": "saved-project",
            "historyMode": "paginated",
            "modelProvider": "pilot_mock",
            "model": "gpt-6.1-sol",
            "reasoningEffort": "medium",
            "createdAt": 1792225092,
            "updatedAt": 1792225093,
            "recencyAt": 1792225092,
            "status": {"type": "idle"},
            "path": r"C:\fleet\home\sessions\2026\10\17\rollout.jsonl",
            "cwd": r"C:\fleet\workspace",
            "cliVersion": "0.163.0",
            "originator": "fixture",
            "source": "vscode",
            "canAcceptDirectInput": true,
            "threadSource": null,
            "agentNickname": null,
            "agentRole": null,
            "gitInfo": null,
            "name": null,
            "daybreakEnabled": null,
            "turns": []
        },
        "readState": null
    })
}

fn windows_thread_resume() -> Value {
    let mut thread = windows_thread_read()["thread"].clone();
    thread["status"] = json!({"type": "active", "activeFlags": []});
    thread["projectId"] = json!("rejoined-project");
    json!({
        "thread": thread,
        "model": "gpt-6.1-sol",
        "modelProvider": "pilot_mock",
        "serviceTier": null,
        "disabledPluginIds": [],
        "cwd": r"C:\fleet\workspace",
        "runtimeWorkspaceRoots": [r"C:\fleet\workspace"],
        "instructionSources": [],
        "approvalPolicy": "never",
        "approvalsReviewer": "user",
        "sandbox": {"type": "readOnly", "networkAccess": false},
        "activePermissionProfile": null,
        "reasoningEffort": "medium",
        "collaborationMode": {
            "mode": "default",
            "settings": {
                "model": "gpt-6.1-sol",
                "reasoning_effort": "medium",
                "developer_instructions": null
            }
        },
        "multiAgentMode": "explicitRequestOnly",
        "initialTurnsPage": null,
        "turnsBackwardsCursor": null,
        "itemsBackwardsCursor": null
    })
}

enum InitialThreadState {
    Loaded,
    Unloaded,
}

async fn enroll_and_rejoin_windows_thread(initial: InitialThreadState) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let loaded = matches!(initial, InitialThreadState::Loaded);
    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut enrollment = None;
        let mut methods = Vec::new();
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
            let method = request["method"].as_str().unwrap();
            methods.push(method.to_owned());
            let result = match method {
                "initialize" => json!({
                    "userAgent": "fixture/1", "codexHome": r"C:\fleet\home",
                    "platformFamily": "windows", "platformOs": "windows"
                }),
                "thread/loaded/list" => json!({
                    "data": if loaded { vec![THREAD_ID] } else { Vec::new() },
                    "nextCursor": null
                }),
                "thread/resume" => windows_thread_resume(),
                "thread/read" => {
                    let mut read = windows_thread_read();
                    // Older responses can omit the nullable project assignment.
                    if !loaded {
                        read["thread"].as_object_mut().unwrap().remove("projectId");
                    }
                    read
                }
                "team/enroll" => {
                    let params: TeamEnrollParams =
                        serde_json::from_value(request["params"].clone())?;
                    let enrolled = TeamEnrollment {
                        address: params.address,
                        alias: params.alias,
                        manually_held: false,
                        continuation_turn_id: None,
                    };
                    enrollment = Some(enrolled.clone());
                    json!({"enrollment": enrolled})
                }
                "team/hostRead" => json!({
                    "installationId": "windows-fixture", "protocolVersion": 7,
                    "control": null, "data": [enrollment.as_ref().unwrap()],
                    "runtime": [], "nextCursor": null
                }),
                method => anyhow::bail!("unexpected fixture request: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": id, "result": result}).to_string().into(),
                ))
                .await?;
        }
        Ok::<_, anyhow::Error>(methods)
    });
    let server = Server {
        environment_capabilities: Default::default(),
        id: "primary".into(),
        label: "Primary".into(),
        endpoint: crate::Endpoint::Websocket {
            url: endpoint.clone(),
            token_file: None,
        },
        cancellation_adapters: Vec::new(),
    };
    let machine = Machine {
        id: "windows".into(),
        label: "Windows".into(),
        servers: vec![server.clone()],
    };
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![machine.clone()];
    let coordinator = Coordinator::open(manifest).await?;
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint,
            auth_token: None,
        },
        client_name: "foreign-metadata-test".into(),
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
    coordinator
        .peers
        .write()
        .await
        .insert("windows/primary".into(), peer.clone());
    let address = TeamAddress {
        machine_id: machine.id.clone(),
        server_id: server.id.clone(),
        thread_id: THREAD_ID.into(),
    };

    let result = async {
        let enrolled = coordinator
            .enroll(
                address.clone(),
                "builder".into(),
                /*operation_id*/ None,
            )
            .await?;
        let expected = Agent {
            discovery: Default::default(),
            enrollment: TeamEnrollment {
                address,
                alias: "builder".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            requested_selection: None,
            runtime: None,
            status: "idle".into(),
            current_model: None,
            current_effort: None,
            next_model: None,
            next_effort: None,
            project: loaded.then(|| "saved-project".into()),
        };
        assert_eq!(enrolled, expected);
        let mut observed = expected.clone();
        observed.discovery.availability.synchronization = TeamSynchronization::Disconnected;
        observed.discovery.availability.reasons = vec![
            "Worker synchronization is unavailable or unconfirmed; host reachability is unknown"
                .into(),
        ];
        assert_eq!(coordinator.agents().await?, vec![observed.clone()]);

        // Discovery after reconnect reads metadata without resuming this thread.
        coordinator
            .refresh_enrollments(&machine, &server, &peer, /*rejoin*/ true)
            .await?;
        assert_eq!(coordinator.agents().await?, vec![observed]);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    client.shutdown().await?;
    let methods = server_task.await??;
    result?;
    let mut expected = vec!["initialize", "thread/loaded/list"];
    if !loaded {
        expected.push("thread/resume");
    }
    expected.extend(["team/enroll", "thread/read", "team/hostRead", "thread/read"]);
    assert_eq!(methods, expected);
    Ok(())
}

#[tokio::test]
async fn loaded_windows_thread_enrollment_and_rejoin_ignore_host_paths() -> Result<()> {
    enroll_and_rejoin_windows_thread(InitialThreadState::Loaded).await
}

#[tokio::test]
async fn unloaded_windows_thread_enrollment_and_rejoin_ignore_host_paths() -> Result<()> {
    enroll_and_rejoin_windows_thread(InitialThreadState::Unloaded).await
}
