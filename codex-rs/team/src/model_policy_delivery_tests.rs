use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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

const CHILD_ID: &str = "00000000-0000-7000-8000-000000000001";

enum SettingsReply {
    Accept,
    Reject,
}

fn resume_response() -> Value {
    json!({
        "thread": {
            "id": CHILD_ID, "environments": [], "sessionId": CHILD_ID,
            "preview": "", "ephemeral": false, "historyMode": "paginated",
            "modelProvider": "fixture", "createdAt": 1, "updatedAt": 1,
            "recencyAt": 1, "status": {"type": "idle"}, "cwd": "/workspace",
            "cliVersion": "test", "source": "vscode", "canAcceptDirectInput": true,
            "turns": []
        },
        "model": "model", "modelProvider": "fixture", "cwd": "/workspace",
        "approvalPolicy": "never", "approvalsReviewer": "user",
        "sandbox": {"type": "readOnly", "networkAccess": false},
        "reasoningEffort": "medium"
    })
}

async fn deliver_to_unloaded_agent(
    mut child: Agent,
    parent: Option<Agent>,
    pending: Option<RequestedSelection>,
    settings_reply: SettingsReply,
    parent_after_sync: Option<Agent>,
) -> Result<(Vec<Value>, Result<()>)> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server_task = tokio::spawn(async move {
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
            let result = match method {
                "initialize" => json!({"userAgent": "fixture/1"}),
                "thread/resume" => {
                    requests.push(json!({"method": method}));
                    resume_response()
                }
                "thread/settings/update" => {
                    let params: ThreadSettingsUpdateParams =
                        serde_json::from_value(request["params"].clone())?;
                    requests.push(json!({
                        "method": method, "threadId": params.thread_id,
                        "model": params.model, "effort": params.effort,
                        "selection": params.team_selection
                    }));
                    json!({})
                }
                "team/deliver" => {
                    let params: TeamDeliverParams =
                        serde_json::from_value(request["params"].clone())?;
                    requests.push(json!({"method": method, "wake": params.delivery.wake}));
                    serde_json::to_value(TeamDeliverResponse {
                        receipt: TeamReceipt {
                            delivery_id: params.delivery.id,
                            thread_id: params.delivery.thread_id,
                            state: TeamReceiptState::Pending,
                            turn_id: None,
                            detail: None,
                        },
                    })?
                }
                method => anyhow::bail!("unexpected fixture request: {method}"),
            };
            let response = if method == "thread/settings/update"
                && matches!(settings_reply, SettingsReply::Reject)
            {
                json!({"id": id, "error": {"code": -32600, "message": "selection rejected"}})
            } else {
                json!({"id": id, "result": result})
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await?;
        }
        Ok::<_, anyhow::Error>(requests)
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
        id: "worker".into(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    };
    let mut manifest = crate::tests::manifest(directory.path());
    manifest.machines = vec![machine.clone()];
    let coordinator = Coordinator::open(manifest).await?;
    child.runtime.as_mut().unwrap().loaded = false;
    let address = child.enrollment.address.clone();
    let key = address_key(&address);
    coordinator.store.put("agent", &key, &child).await?;
    if let Some(parent) = parent {
        coordinator
            .store
            .put("agent", &address_key(&parent.enrollment.address), &parent)
            .await?;
    }
    if let Some(pending) = pending {
        coordinator
            .store
            .put("model_request", &key, &pending)
            .await?;
    }
    let message = coordinator
        .operator_message(
            Some(address),
            "Start the requested task".into(),
            TeamMessageKind::Request,
            /*reply_to*/ None,
            /*topic*/ None,
        )
        .await?;
    coordinator
        .ingest(message, TeamDeliveryMode::NextTurn)
        .await?;
    coordinator.publish().await?;

    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: endpoint,
            auth_token: None,
        },
        client_name: "model-wake-test".into(),
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

    // Match worker synchronization, then recheck the latest selection before
    // resuming the recipient because readiness can dispatch its stored mail.
    let result = async {
        coordinator.inherit_models(&machine, &server, &peer).await?;
        if let Some(parent) = parent_after_sync {
            coordinator
                .store
                .put("agent", &address_key(&parent.enrollment.address), &parent)
                .await?;
        }
        coordinator
            .deliver_to(&machine.id, &server.id, &peer, &HashMap::new())
            .await
    }
    .await;
    client.shutdown().await?;
    let requests = server_task.await??;
    Ok((requests, result))
}

fn explicit_selection() -> RequestedSelection {
    RequestedSelection {
        selection: TeamModelSelection {
            revision: 7,
            model: TeamSelection::Value {
                value: "selected-model".into(),
            },
            effort: TeamSelection::Value {
                value: "high".into(),
            },
        },
        model: Some("selected-model".into()),
        effort: Some("high".into()),
    }
}

#[tokio::test]
async fn unloaded_agent_applies_pending_model_and_effort_before_first_wake() -> Result<()> {
    let child = tests::agent(
        CHILD_ID,
        None,
        TeamSelectionOrigin::Configured,
        TeamSelectionOrigin::Configured,
    );
    let pending = explicit_selection();
    let (requests, result) = deliver_to_unloaded_agent(
        child,
        /*parent*/ None,
        Some(pending.clone()),
        SettingsReply::Accept,
        /*parent_after_sync*/ None,
    )
    .await?;
    result?;
    assert_eq!(
        requests,
        vec![
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": "selected-model", "effort": "high", "selection": pending.selection
            }),
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": "selected-model", "effort": "high", "selection": pending.selection
            }),
            json!({"method": "thread/resume"}),
            json!({"method": "team/deliver", "wake": true}),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn unloaded_agent_applies_pending_inherited_default_effort_and_preserves_role_model()
-> Result<()> {
    let child = tests::agent(
        CHILD_ID,
        Some("parent"),
        TeamSelectionOrigin::Role,
        TeamSelectionOrigin::Inherited,
    );
    let mut parent = tests::agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Configured,
    );
    parent.next_model = Some("parent-model".into());
    parent.next_effort = None;
    let (requests, result) = deliver_to_unloaded_agent(
        child,
        Some(parent),
        Some(RequestedSelection {
            selection: TeamModelSelection {
                revision: 1,
                model: TeamSelection::Keep,
                effort: TeamSelection::Inherit,
            },
            model: None,
            effort: None,
        }),
        SettingsReply::Accept,
        /*parent_after_sync*/ None,
    )
    .await?;
    result?;
    assert_eq!(
        requests,
        vec![
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": null, "effort": null,
                "selection": TeamModelSelection {
                    revision: 1, model: TeamSelection::Keep, effort: TeamSelection::Inherit
                }
            }),
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": null, "effort": null,
                "selection": TeamModelSelection {
                    revision: 1, model: TeamSelection::Keep, effort: TeamSelection::Inherit
                }
            }),
            json!({"method": "thread/resume"}),
            json!({"method": "team/deliver", "wake": true}),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn unloaded_agent_does_not_reapply_applied_or_superseded_selection() -> Result<()> {
    for (applied, superseded) in [(Some(7), None), (None, Some(8))] {
        let mut child = tests::agent(
            CHILD_ID,
            None,
            TeamSelectionOrigin::Inherited,
            TeamSelectionOrigin::Inherited,
        );
        let runtime = child.runtime.as_mut().unwrap();
        runtime.applied_selection_revision = applied;
        runtime.superseded_selection_revision = superseded;
        let (requests, result) = deliver_to_unloaded_agent(
            child,
            /*parent*/ None,
            Some(explicit_selection()),
            SettingsReply::Accept,
            /*parent_after_sync*/ None,
        )
        .await?;
        result?;
        assert_eq!(
            requests,
            vec![
                json!({"method": "thread/resume"}),
                json!({"method": "team/deliver", "wake": true}),
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn unloaded_agent_does_not_wake_when_pending_settings_are_rejected() -> Result<()> {
    let child = tests::agent(
        CHILD_ID,
        None,
        TeamSelectionOrigin::Configured,
        TeamSelectionOrigin::Configured,
    );
    let pending = explicit_selection();
    let (requests, result) = deliver_to_unloaded_agent(
        child,
        /*parent*/ None,
        Some(pending.clone()),
        SettingsReply::Reject,
        /*parent_after_sync*/ None,
    )
    .await?;
    assert!(
        result
            .expect_err("rejected settings must stop delivery")
            .to_string()
            .contains("selection rejected")
    );
    assert_eq!(
        requests,
        vec![json!({
            "method": "thread/settings/update", "threadId": CHILD_ID,
            "model": "selected-model", "effort": "high", "selection": pending.selection
        }),]
    );
    Ok(())
}

#[tokio::test]
async fn unloaded_agent_refreshes_pending_inheritance_before_first_wake() -> Result<()> {
    let child = tests::agent(
        CHILD_ID,
        Some("parent"),
        TeamSelectionOrigin::Inherited,
        TeamSelectionOrigin::Inherited,
    );
    let mut parent = tests::agent(
        "parent",
        None,
        TeamSelectionOrigin::Explicit,
        TeamSelectionOrigin::Explicit,
    );
    parent.next_model = Some("parent-before".into());
    parent.next_effort = Some("low".into());
    let mut latest = parent.clone();
    latest.next_model = Some("parent-after".into());
    latest.next_effort = Some("high".into());
    let pending = RequestedSelection {
        selection: TeamModelSelection {
            revision: 1,
            model: TeamSelection::Inherit,
            effort: TeamSelection::Inherit,
        },
        model: parent.next_model.clone(),
        effort: parent.next_effort.clone(),
    };
    let (requests, result) = deliver_to_unloaded_agent(
        child,
        Some(parent),
        Some(pending),
        SettingsReply::Accept,
        Some(latest),
    )
    .await?;
    result?;
    assert_eq!(
        requests,
        vec![
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": "parent-before", "effort": "low",
                "selection": TeamModelSelection {
                    revision: 1, model: TeamSelection::Inherit, effort: TeamSelection::Inherit
                }
            }),
            json!({
                "method": "thread/settings/update", "threadId": CHILD_ID,
                "model": "parent-after", "effort": "high",
                "selection": TeamModelSelection {
                    revision: 2, model: TeamSelection::Inherit, effort: TeamSelection::Inherit
                }
            }),
            json!({"method": "thread/resume"}),
            json!({"method": "team/deliver", "wake": true}),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn archived_inherited_sibling_does_not_block_live_pending_selection() -> Result<()> {
    let child = tests::agent(
        CHILD_ID,
        None,
        TeamSelectionOrigin::Configured,
        TeamSelectionOrigin::Configured,
    );
    let mut archived = tests::agent(
        "archived",
        Some("missing-parent"),
        TeamSelectionOrigin::Inherited,
        TeamSelectionOrigin::Inherited,
    );
    archived.runtime.as_mut().unwrap().archived = true;
    let (requests, result) = deliver_to_unloaded_agent(
        child,
        Some(archived),
        Some(explicit_selection()),
        SettingsReply::Accept,
        /*parent_after_sync*/ None,
    )
    .await?;
    result?;
    assert_eq!(
        requests
            .iter()
            .map(|request| request["method"].clone())
            .collect::<Vec<_>>(),
        vec![
            "thread/settings/update",
            "thread/settings/update",
            "thread/resume",
            "team/deliver"
        ]
    );
    Ok(())
}
