use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::*;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_cancels_only_the_owned_structured_job_and_confirms_its_result() -> Result<()> {
    let mcp = MockServer::start().await;
    Mock::given(method("POST")).respond_with(|request: &wiremock::Request| {
        let request: Value = serde_json::from_slice(&request.body).unwrap();
        let result = match request["method"].as_str().unwrap_or_default() {
            "initialize" => json!({"protocolVersion":"2025-06-18", "capabilities":{"tools":{}}, "serverInfo":{"name":"jobs", "version":"test"}}),
            "tools/list" => json!({"tools":[
                {"name":"launch", "description":"Launch owned work", "inputSchema":{"type":"object","properties":{}}},
                {"name":"cancel", "description":"Cancel owned work", "inputSchema":{"type":"object","properties":{"job_id":{"type":"string"}},"required":["job_id"]}}
            ]}),
            "tools/call" if request["params"]["name"] == "launch" => json!({"content":[{"type":"text","text":"An unrelated job is other-id"}], "structuredContent":{"job_id":"owned-id"},"isError":false}),
            "tools/call" if request["params"]["name"] == "cancel" => json!({"content":[],"structuredContent":{"cancelled":true},"isError":false}),
            _ => return ResponseTemplate::new(202),
        };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
    }).mount(&mcp).await;
    let (_release, blocked) = oneshot::channel();
    let (model, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("launch"),
                responses::ev_function_call_with_namespace(
                    "launch-job",
                    "mcp__jobs",
                    "launch",
                    "{}",
                ),
                responses::ev_completed("launch"),
            ]),
        }],
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created("working")]),
            },
            StreamingSseChunk {
                gate: Some(blocked),
                body: responses::sse(vec![responses::ev_completed("working")]),
            },
        ],
    ])
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(model.uri())
        .with_extra_config(&format!(
            "[mcp_servers.jobs]\nurl = {:?}\ndefault_tools_approval_mode = \"approve\"\n",
            mcp.uri()
        ))
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let control = TeamControlState {
        coordinator_id: "test-coordinator".into(),
        revision: 1,
        mode: TeamExecutionMode::Running,
    };
    let _: TeamEnrollResponse = app
        .request(|request_id| ClientRequest::TeamEnroll {
            request_id,
            params: TeamEnrollParams {
                address: TeamAddress {
                    machine_id: "local".into(),
                    server_id: "primary".into(),
                    thread_id: thread.id.clone(),
                },
                alias: "builder".into(),
                control: control.clone(),
            },
        })
        .await?;
    let _: TeamSyncResponse = app
        .request(|request_id| ClientRequest::TeamSync {
            request_id,
            params: TeamSyncParams {
                coordinator_id: control.coordinator_id.clone(),
                directory: Vec::new(),
                board: Vec::new(),
                cancellation_adapters: vec![TeamCancellationAdapter {
                    server: "jobs".into(),
                    plugin_id: None,
                    launch_tool: "launch".into(),
                    job_id_pointer: "/job_id".into(),
                    cancel_tool: "cancel".into(),
                    cancel_id_field: "job_id".into(),
                    cancel_arguments: json!({}),
                    confirmation_pointer: "/cancelled".into(),
                    confirmation_value: json!(true),
                }],
            },
        })
        .await?;
    let _: TurnStartResponse = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Launch work".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    loop {
        let completed: ItemCompletedNotification = app.read_notification("item/completed").await?;
        if let ThreadItem::McpToolCall {
            status,
            result,
            error,
            ..
        } = completed.item
        {
            assert_eq!(error, None, "launch must reach the mock job service");
            assert_eq!(status, McpToolCallStatus::Completed);
            assert_eq!(
                result.unwrap().structured_content,
                Some(json!({"job_id":"owned-id"}))
            );
            break;
        }
    }
    let _: TeamControlResponse = app
        .request(|request_id| ClientRequest::TeamControl {
            request_id,
            params: TeamControlParams {
                control: TeamControlState {
                    revision: 2,
                    mode: TeamExecutionMode::Stopped,
                    ..control
                },
            },
        })
        .await?;
    let observed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let host: TeamHostReadResponse = app
                .request(|request_id| ClientRequest::TeamHostRead {
                    request_id,
                    params: TeamHostReadParams {
                        cursor: None,
                        limit: None,
                    },
                })
                .await?;
            if let Some(job) = host
                .runtime
                .iter()
                .flat_map(|member| &member.jobs)
                .find(|job| job.status == TeamJobStatus::Cancelled)
            {
                return Ok::<_, anyhow::Error>(job.clone());
            }
            let _: TeamChangedNotification = app.read_notification("team/changed").await?;
        }
    })
    .await??;
    assert_eq!(observed.handle.as_deref(), Some("owned-id"));
    assert_eq!(observed.status, TeamJobStatus::Cancelled);
    let cancels = mcp
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .filter(|request| {
            request["method"] == "tools/call" && request["params"]["name"] == "cancel"
        })
        .collect::<Vec<_>>();
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        cancels[0]["params"]["arguments"],
        json!({"job_id":"owned-id"})
    );
    Ok(())
}
