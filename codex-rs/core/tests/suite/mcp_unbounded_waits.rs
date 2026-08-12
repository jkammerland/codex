use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_features::Feature;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::user_input::UserInput;
use core_test_support::fs_wait::wait_for_path_exists;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use wiremock::MockServer;

use super::rmcp_client::remote_aware_environment_id;
use super::rmcp_client::remote_aware_stdio_server_bin;

const MCP_SERVER: &str = "unbounded_wait";
const TOOL: &str = "sync";
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

enum DispatchPath {
    Direct,
    CodeMode,
}

async fn build_fixture(
    server: &MockServer,
    tool_timeout_sec: Option<Duration>,
    dispatch_path: DispatchPath,
) -> Result<TestCodex> {
    let command = remote_aware_stdio_server_bin()?;
    let environment_id = remote_aware_environment_id();
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(move |config| {
            if matches!(dispatch_path, DispatchPath::CodeMode) {
                config
                    .features
                    .enable(Feature::CodeMode)
                    .expect("Code Mode should be enabled for nested MCP coverage");
            }
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                MCP_SERVER.to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command,
                        args: Vec::new(),
                        env: None,
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    environment_id,
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test MCP servers should accept any configuration");
        });
    let fixture = builder.build_with_auto_env(server).await?;
    wait_for_mcp_server(&fixture.codex, MCP_SERVER).await?;
    Ok(fixture)
}

fn sync_arguments(
    started_file: Option<&Path>,
    release_file: Option<&Path>,
    barrier_id: Option<&str>,
) -> Value {
    let mut arguments = json!({});
    if let Some(started_file) = started_file {
        arguments["started_file"] = Value::String(started_file.to_string_lossy().into_owned());
    }
    if let Some(release_file) = release_file {
        arguments["release_file"] = Value::String(release_file.to_string_lossy().into_owned());
    }
    if let Some(barrier_id) = barrier_id {
        arguments["barrier"] = json!({
            "id": barrier_id,
            "participants": 2,
            "timeout_ms": 60_000,
        });
    }
    arguments
}

fn code_mode_sync_source(arguments: &Value) -> String {
    format!(
        r#"
const pending = tools.mcp__{MCP_SERVER}__{TOOL}({arguments});
yield_control();
const result = await pending;
text(JSON.stringify(result));
"#
    )
}

fn running_cell_id(request: &ResponsesRequest, call_id: &str) -> String {
    let output = request.custom_tool_call_output(call_id);
    let text = match output.get("output") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str),
        Some(Value::String(text)) => Some(text.as_str()),
        _ => None,
    }
    .expect("Code Mode should return a running-cell header");
    text.strip_prefix("Script running with cell ID ")
        .and_then(|rest| rest.split('\n').next())
        .expect("running-cell header should include a cell ID")
        .to_string()
}

async fn start_code_mode_sync_call(
    server: &MockServer,
    fixture: &TestCodex,
    call_id: &str,
    arguments: &Value,
    started_file: &Path,
) -> Result<(String, ResponseMock)> {
    responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_response_created("resp-unbounded-wait-exec"),
            responses::ev_custom_tool_call(call_id, "exec", &code_mode_sync_source(arguments)),
            responses::ev_completed("resp-unbounded-wait-exec"),
        ]),
    )
    .await;
    let initial_completion = responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_assistant_message("msg-unbounded-waiting", "waiting"),
            responses::ev_completed("resp-unbounded-waiting"),
        ]),
    )
    .await;
    fixture
        .submit_turn("start the blocking nested MCP call")
        .await?;
    wait_for_path_exists(started_file, Duration::from_secs(5)).await?;
    let cell_id = running_cell_id(&initial_completion.single_request(), call_id);

    Ok((cell_id, initial_completion))
}

async fn wait_for_code_mode_cell(
    server: &MockServer,
    fixture: &TestCodex,
    cell_id: &str,
    expected_call_id: &str,
) -> Result<ResponseMock> {
    responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_response_created("resp-unbounded-wait"),
            responses::ev_function_call(
                expected_call_id,
                "wait",
                &serde_json::to_string(&json!({
                    "cell_id": cell_id,
                    "yield_time_ms": 10_000,
                }))?,
            ),
            responses::ev_completed("resp-unbounded-wait"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_assistant_message("msg-unbounded-done", "done"),
            responses::ev_completed("resp-unbounded-done"),
        ]),
    )
    .await;
    fixture
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "collect the nested MCP result".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(
            event,
            EventMsg::RawResponseItem(raw)
                if matches!(&raw.item, ResponseItem::FunctionCall { call_id, .. }
                    if call_id == expected_call_id)
        )
    })
    .await;
    Ok(completion)
}

async fn advance_past_unbounded_wait_boundary() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1201)).await;
    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
}

fn resume_time() {
    tokio::time::resume();
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn direct_mcp_zero_timeout_stays_pending_past_one_day() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::Direct).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("direct-unbounded.started");
    let release_file = temp_dir.path().join("direct-unbounded.release");
    let arguments = sync_arguments(
        Some(&started_file),
        Some(&release_file),
        /*barrier_id*/ None,
    );
    let call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let arguments = arguments.clone();
        async move {
            codex
                .call_mcp_tool(MCP_SERVER, TOOL, Some(arguments), /*meta*/ None)
                .await
        }
    });
    wait_for_path_exists(&started_file, Duration::from_secs(5)).await?;

    advance_past_unbounded_wait_boundary().await;
    let pending = !call.is_finished();
    resume_time();
    assert!(pending, "zero timeout must not end a direct MCP call");

    std::fs::write(&release_file, "release")?;
    let result = call.await??;
    assert_eq!(result.structured_content, Some(json!({ "result": "ok" })));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn direct_mcp_omitted_timeout_keeps_the_finite_default() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, None, DispatchPath::Direct).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("direct-default-timeout.started");
    let call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let started_file = started_file.clone();
        async move {
            codex
                .call_mcp_tool(
                    MCP_SERVER,
                    TOOL,
                    Some(sync_arguments(
                        Some(&started_file),
                        /*release_file*/ None,
                        Some("direct-default-timeout"),
                    )),
                    /*meta*/ None,
                )
                .await
        }
    });
    wait_for_path_exists(&started_file, Duration::from_secs(5)).await?;

    tokio::time::pause();
    tokio::time::advance(DEFAULT_TOOL_TIMEOUT + Duration::from_secs(1)).await;
    resume_time();
    let error = call
        .await?
        .expect_err("the omitted timeout should retain its finite deadline");
    assert!(
        format!("{error:#}").contains("timed out awaiting tools/call after 300s"),
        "omitted timeout should preserve the default timeout origin: {error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn direct_mcp_positive_timeout_reports_a_timeout() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(TOOL_TIMEOUT), DispatchPath::Direct).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("direct-positive-timeout.started");
    let call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let started_file = started_file.clone();
        async move {
            codex
                .call_mcp_tool(
                    MCP_SERVER,
                    TOOL,
                    Some(sync_arguments(
                        Some(&started_file),
                        /*release_file*/ None,
                        Some("direct-positive-timeout"),
                    )),
                    /*meta*/ None,
                )
                .await
        }
    });
    wait_for_path_exists(&started_file, Duration::from_secs(5)).await?;

    tokio::time::pause();
    tokio::time::advance(TOOL_TIMEOUT + Duration::from_secs(1)).await;
    resume_time();
    let error = call
        .await?
        .expect_err("positive timeout should end the MCP call");
    assert!(
        format!("{error:#}").contains("timed out awaiting tools/call"),
        "positive timeout should preserve its timeout origin: {error:#}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn direct_mcp_transport_loss_returns_an_error_without_replaying_the_tool_call() -> Result<()>
{
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::Direct).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("transport-loss.started");
    let exit_file = temp_dir.path().join("transport-loss.exit");
    let call_count_file = temp_dir.path().join("transport-loss.calls");
    let call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let arguments = json!({
            "started_file": started_file,
            "exit_file": exit_file,
            "call_count_file": call_count_file,
        });
        async move {
            codex
                .call_mcp_tool(MCP_SERVER, TOOL, Some(arguments), /*meta*/ None)
                .await
        }
    });
    wait_for_path_exists(&started_file, Duration::from_secs(5)).await?;
    std::fs::write(&exit_file, "exit")?;

    let error = call
        .await?
        .expect_err("closing the stdio server must fail the active MCP call");
    let error_text = format!("{error:#}");
    assert!(
        error_text.contains("Transport closed"),
        "transport loss should remain distinguishable from a timeout: {error_text}"
    );
    assert!(
        !error_text.contains("timed out awaiting tools/call"),
        "transport loss must not be reported as a tool deadline: {error_text}"
    );
    assert_eq!(
        std::fs::read_to_string(&call_count_file)?.lines().count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_turn_interrupt_aborts_an_unbounded_mcp_call_without_a_follow_up_request()
-> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("interrupted-unbounded.started");
    let release_file = temp_dir.path().join("interrupted-unbounded.release");
    let arguments = sync_arguments(
        Some(&started_file),
        Some(&release_file),
        /*barrier_id*/ None,
    );
    let first_response = responses::mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-unbounded-interrupt"),
            responses::ev_function_call_with_namespace(
                "call-unbounded-interrupt",
                &format!("mcp__{MCP_SERVER}"),
                "sync_readonly",
                &serde_json::to_string(&arguments)?,
            ),
            responses::ev_completed("resp-unbounded-interrupt"),
        ]),
    )
    .await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::Direct).await?;

    fixture
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start an unbounded MCP call".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(
            event,
            EventMsg::McpToolCallBegin(begin) if begin.call_id == "call-unbounded-interrupt"
        )
    })
    .await;
    wait_for_path_exists(&started_file, Duration::from_secs(5)).await?;

    fixture.codex.submit(Op::Interrupt).await?;
    let abort = wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let EventMsg::TurnAborted(abort) = abort else {
        unreachable!("event guard guarantees a turn-aborted event");
    };
    assert_eq!(abort.reason, TurnAbortReason::Interrupted);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        first_response.requests().len(),
        1,
        "an explicit interrupt must not replay the MCP tool call or issue a follow-up model request"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn code_mode_mcp_zero_timeout_stays_pending_without_an_intermediate_turn() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::CodeMode).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join("code-mode-unbounded.started");
    let release_file = temp_dir.path().join("code-mode-unbounded.release");
    let arguments = sync_arguments(
        Some(&started_file),
        Some(&release_file),
        /*barrier_id*/ None,
    );
    let (cell_id, initial_completion) = start_code_mode_sync_call(
        &server,
        &fixture,
        "call-code-mode-unbounded",
        &arguments,
        &started_file,
    )
    .await?;

    let request_count_before_advance = initial_completion.requests().len();
    advance_past_unbounded_wait_boundary().await;
    let no_intermediate_turn = initial_completion.requests().len() == request_count_before_advance;
    resume_time();
    assert!(
        no_intermediate_turn,
        "an unbounded nested MCP call must not return a nonterminal exec result to the model"
    );

    std::fs::write(&release_file, "release")?;
    let completion =
        wait_for_code_mode_cell(&server, &fixture, &cell_id, "call-code-mode-wait").await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output = completion
        .single_request()
        .function_call_output("call-code-mode-wait")
        .to_string();
    assert!(
        output.contains("ok"),
        "nested MCP output was unexpected: {output}"
    );
    Ok(())
}

async fn assert_code_mode_timeout(
    tool_timeout_sec: Option<Duration>,
    elapsed: Duration,
    barrier_id: &str,
    call_id: &str,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, tool_timeout_sec, DispatchPath::CodeMode).await?;
    let temp_dir = tempfile::tempdir()?;
    let started_file = temp_dir.path().join(format!("{barrier_id}.started"));
    let arguments = sync_arguments(
        Some(&started_file),
        /*release_file*/ None,
        Some(barrier_id),
    );
    let (cell_id, initial_completion) =
        start_code_mode_sync_call(&server, &fixture, call_id, &arguments, &started_file).await?;

    let request_count_before_advance = initial_completion.requests().len();
    tokio::time::pause();
    tokio::time::advance(elapsed).await;
    resume_time();
    assert_eq!(
        initial_completion.requests().len(),
        request_count_before_advance
    );
    let completion =
        wait_for_code_mode_cell(&server, &fixture, &cell_id, "call-code-mode-wait").await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let output = completion
        .single_request()
        .function_call_output("call-code-mode-wait")
        .to_string();
    assert!(
        output.contains("timed out awaiting tools/call"),
        "nested MCP timeout should stay typed as a tool deadline: {output}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn code_mode_mcp_omitted_timeout_keeps_the_finite_default() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    assert_code_mode_timeout(
        /*tool_timeout_sec*/ None,
        DEFAULT_TOOL_TIMEOUT + Duration::from_secs(1),
        "code-mode-default-timeout",
        "call-code-mode-default-timeout",
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn code_mode_mcp_positive_timeout_reports_a_timeout() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    assert_code_mode_timeout(
        Some(TOOL_TIMEOUT),
        TOOL_TIMEOUT + Duration::from_secs(1),
        "code-mode-positive-timeout",
        "call-code-mode-positive-timeout",
    )
    .await
}
