use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolFunctionSpec;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadAgentWorkUpdatedNotification;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(30);

#[test_case::test_case(false; "feature_disabled")]
#[test_case::test_case(true; "feature_enabled")]
#[tokio::test]
async fn snapshots_preserve_agent_work_availability_across_cold_resume(
    enabled: bool,
) -> Result<()> {
    let model = responses::start_mock_server().await;
    let response = responses::mount_sse_once(
        &model,
        responses::sse(vec![
            responses::ev_response_created("response"),
            responses::ev_assistant_message("message", "Ready."),
            responses::ev_completed("response"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    let config = MockResponsesConfig::new(&model.uri()).enable_feature(Feature::MultiAgentV2);
    let config = if enabled {
        config.enable_feature(Feature::WakeOnAgentMessages)
    } else {
        config.disable_feature(Feature::WakeOnAgentMessages)
    };
    config.write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    assert_eq!(thread.agent_work.is_some(), enabled);
    if let Some(work) = &thread.agent_work {
        assert_eq!((work.pending_mail, &work.waiting_tasks), (0, &None));
    }
    let expected = thread
        .agent_work
        .map(|work| (work.pending_mail, work.waiting_tasks));

    // Persist a real user turn so cold resume reconstructs the thread from disk.
    timeout(
        TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Make this thread resumable.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        }),
    )
    .await??;
    let read: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(
        read.thread
            .agent_work
            .map(|work| (work.pending_mail, work.waiting_tasks)),
        expected
    );
    assert!(
        timeout(TIMEOUT, app.shutdown_gracefully())
            .await??
            .success()
    );
    drop(app);

    let mut reopened = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let resumed: ThreadResumeResponse = reopened
        .request(|request_id| ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: thread.id.clone(),
                ..Default::default()
            },
        })
        .await?;
    assert_eq!(
        resumed
            .thread
            .agent_work
            .as_ref()
            .map(|work| (work.pending_mail, work.waiting_tasks.clone())),
        expected
    );
    let read: ThreadReadResponse = reopened
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(read.thread.agent_work, resumed.thread.agent_work);
    let list_request = reopened
        .send_raw_request("thread/list", Some(json!({"limit": 10})))
        .await?;
    let listed: ThreadListResponse =
        timeout(TIMEOUT, reopened.read_response(list_request)).await??;
    let listed = listed
        .data
        .into_iter()
        .find(|listed| listed.id == thread.id)
        .context("resumed thread in list")?;
    assert_eq!(listed.agent_work, resumed.thread.agent_work);
    assert_eq!(response.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn live_agent_mail_counts_remain_pending_until_the_model_request_includes_mail() -> Result<()>
{
    let model = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &model,
        vec![
            responses::sse(vec![
                responses::ev_response_created("before-mail"),
                responses::ev_function_call_with_namespace(
                    "send-mail",
                    "collaboration",
                    "send_message",
                    &json!({"target": "/root", "message": "queued native mail", "wake": false})
                        .to_string(),
                ),
                responses::ev_function_call("hold-request", "mail_gate", "{}"),
                responses::ev_completed("before-mail"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("after-mail"),
                responses::ev_assistant_message("done", "Mail received."),
                responses::ev_completed("after-mail"),
            ]),
        ],
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&model.uri())
        .enable_feature(Feature::MultiAgentV2)
        .enable_feature(Feature::WakeOnAgentMessages)
        .disable_feature(Feature::CodeMode)
        .disable_feature(Feature::CodeModeOnly)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            dynamic_tools: Some(vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "mail_gate".into(),
                description: "Hold the next model request until the client responds.".into(),
                input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                defer_loading: false,
            })]),
            ..Default::default()
        })
        .await?
        .thread;
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Queue native mail and wait at the gate.".into(),
                    text_elements: vec![],
                }],
                ..Default::default()
            },
        })
        .await?;

    // The dynamic tool is a deterministic gate: accepted mail cannot reach the
    // next model request until the test answers the public tool-call request.
    let (gate, pending) = timeout(TIMEOUT, async {
        let mut gate = None;
        let mut pending = None;
        loop {
            match app.read_next_message().await? {
                JSONRPCMessage::Request(request) => {
                    let request: ServerRequest = request.try_into()?;
                    match request {
                        ServerRequest::DynamicToolCall { request_id, params } => {
                            assert_eq!(params.tool, "mail_gate");
                            gate = Some(request_id);
                        }
                        other => anyhow::bail!("unexpected request: {other:?}"),
                    }
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "thread/agentWork/updated" =>
                {
                    let update: ThreadAgentWorkUpdatedNotification = serde_json::from_value(
                        notification.params.context("agent-work parameters")?,
                    )?;
                    assert_eq!(update.thread_id, thread.id);
                    let work = update.agent_work.context("enabled agent-work snapshot")?;
                    if work.pending_mail == 1 {
                        pending = Some(work);
                    }
                }
                JSONRPCMessage::Error(error) => anyhow::bail!("unexpected RPC error: {error:?}"),
                JSONRPCMessage::Notification(_) | JSONRPCMessage::Response(_) => {}
            }
            if let (Some(gate), Some(pending)) = (&gate, &pending) {
                break Ok::<_, anyhow::Error>((gate.clone(), pending.clone()));
            }
        }
    })
    .await??;
    let read: ThreadReadResponse = app
        .request(|request_id| ClientRequest::ThreadRead {
            request_id,
            params: ThreadReadParams {
                thread_id: thread.id.clone(),
                include_turns: false,
            },
        })
        .await?;
    assert_eq!(read.thread.agent_work, Some(pending.clone()));
    assert_eq!(requests.requests().len(), 1);
    app.send_response(
        gate,
        serde_json::to_value(DynamicToolCallResponse {
            content_items: vec![],
            success: true,
        })?,
    )
    .await?;

    let consumed = timeout(TIMEOUT, async {
        let mut completed = false;
        let mut consumed = None;
        loop {
            match app.read_next_message().await? {
                JSONRPCMessage::Notification(notification)
                    if notification.method == "thread/agentWork/updated" =>
                {
                    let update: ThreadAgentWorkUpdatedNotification = serde_json::from_value(
                        notification.params.context("agent-work parameters")?,
                    )?;
                    assert_eq!(update.thread_id, thread.id);
                    let work = update.agent_work.context("enabled agent-work snapshot")?;
                    if work.pending_mail == 0 {
                        consumed = Some(work);
                    }
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "turn/completed" =>
                {
                    completed = true;
                }
                JSONRPCMessage::Error(error) => anyhow::bail!("unexpected RPC error: {error:?}"),
                JSONRPCMessage::Request(request) => {
                    anyhow::bail!("unexpected request: {request:?}")
                }
                JSONRPCMessage::Notification(_) | JSONRPCMessage::Response(_) => {}
            }
            if completed && let Some(work) = consumed.take() {
                break Ok::<_, anyhow::Error>(work);
            }
        }
    })
    .await??;
    assert!(consumed.revision > pending.revision);
    assert_eq!(consumed.waiting_tasks, None);
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body_contains_text("queued native mail"));
    Ok(())
}
