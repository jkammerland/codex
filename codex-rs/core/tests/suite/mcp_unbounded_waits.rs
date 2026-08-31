use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::fs_wait::wait_for_path_exists;
use core_test_support::responses;
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
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy)]
enum DispatchPath {
    Direct,
    CodeMode,
}

struct BlockingCall {
    started_file: PathBuf,
    release_file: PathBuf,
    call_count_file: PathBuf,
    arguments: Value,
}

impl BlockingCall {
    fn new(temp_dir: &tempfile::TempDir, name: &str) -> Self {
        let started_file = temp_dir.path().join(format!("{name}.started"));
        let release_file = temp_dir.path().join(format!("{name}.release"));
        let call_count_file = temp_dir.path().join(format!("{name}.calls"));
        let arguments = json!({
            "started_file": started_file,
            "release_file": release_file,
            "call_count_file": call_count_file,
        });
        Self {
            started_file,
            release_file,
            call_count_file,
            arguments,
        }
    }

    fn count(&self) -> Result<usize> {
        Ok(std::fs::read_to_string(&self.call_count_file)?
            .lines()
            .count())
    }

    fn release(&self) -> Result<()> {
        std::fs::write(&self.release_file, "release")?;
        Ok(())
    }
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

async fn advance_past_one_day() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1_201)).await;
    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
}

fn code_mode_source(arguments: &Value) -> String {
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

async fn start_code_mode_call(
    server: &MockServer,
    fixture: &TestCodex,
    call_id: &str,
    blocking_call: &BlockingCall,
) -> Result<String> {
    responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_response_created("resp-unbounded-exec"),
            responses::ev_custom_tool_call(
                call_id,
                "exec",
                &code_mode_source(&blocking_call.arguments),
            ),
            responses::ev_completed("resp-unbounded-exec"),
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

    fixture.submit_turn("start the nested MCP call").await?;
    wait_for_path_exists(&blocking_call.started_file, Duration::from_secs(5)).await?;
    let cell_id = running_cell_id(&initial_completion.single_request(), call_id);
    Ok(cell_id)
}

async fn collect_code_mode_cell(
    server: &MockServer,
    fixture: &TestCodex,
    cell_id: &str,
) -> Result<String> {
    responses::mount_sse_once(
        server,
        sse(vec![
            responses::ev_response_created("resp-unbounded-wait"),
            responses::ev_function_call(
                "call-unbounded-wait",
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
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "collect the nested MCP result".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&fixture.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(completion
        .single_request()
        .function_call_output("call-unbounded-wait")
        .to_string())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn direct_zero_timeout_stays_pending_past_one_day_and_executes_once() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::Direct).await?;
    let temp_dir = tempfile::tempdir()?;
    let blocking_call = BlockingCall::new(&temp_dir, "direct-zero");
    let call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let arguments = blocking_call.arguments.clone();
        async move {
            codex
                .call_mcp_tool(MCP_SERVER, TOOL, Some(arguments), /*meta*/ None)
                .await
        }
    });
    wait_for_path_exists(&blocking_call.started_file, Duration::from_secs(5)).await?;

    advance_past_one_day().await;
    let pending = !call.is_finished();
    let call_count = blocking_call.count()?;
    tokio::time::resume();
    assert!(pending, "zero timeout must not end the direct MCP call");
    assert_eq!(call_count, 1, "the ambiguous MCP call must not replay");

    blocking_call.release()?;
    let result = call.await??;
    assert_eq!(result.structured_content, Some(json!({ "result": "ok" })));
    assert_eq!(blocking_call.count()?, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn omitted_and_positive_direct_timeouts_remain_finite() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    for (name, configured_timeout, effective_timeout) in [
        ("omitted", None, DEFAULT_TOOL_TIMEOUT),
        (
            "positive",
            Some(Duration::from_secs(60)),
            Duration::from_secs(60),
        ),
    ] {
        let server = responses::start_mock_server().await;
        let fixture = build_fixture(&server, configured_timeout, DispatchPath::Direct).await?;
        let temp_dir = tempfile::tempdir()?;
        let blocking_call = BlockingCall::new(&temp_dir, name);
        let call = tokio::spawn({
            let codex = Arc::clone(&fixture.codex);
            let arguments = blocking_call.arguments.clone();
            async move {
                codex
                    .call_mcp_tool(MCP_SERVER, TOOL, Some(arguments), /*meta*/ None)
                    .await
            }
        });
        wait_for_path_exists(&blocking_call.started_file, Duration::from_secs(5)).await?;

        tokio::time::pause();
        tokio::time::advance(effective_timeout + Duration::from_secs(1)).await;
        tokio::time::resume();
        blocking_call.release()?;
        let error = call.await?.expect_err("finite MCP timeout should expire");
        assert!(
            format!("{error:#}").contains("timed out awaiting tools/call"),
            "{name} timeout lost its deadline-specific error: {error:#}"
        );
        assert_eq!(blocking_call.count()?, 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn code_mode_zero_timeout_survives_yield_and_one_day_without_a_model_turn() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let fixture = build_fixture(&server, Some(Duration::ZERO), DispatchPath::CodeMode).await?;
    let temp_dir = tempfile::tempdir()?;
    let blocking_call = BlockingCall::new(&temp_dir, "code-mode-zero");
    let cell_id =
        start_code_mode_call(&server, &fixture, "call-code-mode-zero", &blocking_call).await?;
    let request_count = server
        .received_requests()
        .await
        .expect("mock server should expose received requests")
        .len();

    advance_past_one_day().await;
    let unchanged_request_count = server
        .received_requests()
        .await
        .expect("mock server should expose received requests")
        .len();
    let call_count = blocking_call.count()?;
    tokio::time::resume();
    assert_eq!(unchanged_request_count, request_count);
    assert_eq!(call_count, 1, "the nested MCP call must not replay");

    blocking_call.release()?;
    let output = collect_code_mode_cell(&server, &fixture, &cell_id).await?;
    assert!(output.contains("ok"), "unexpected nested MCP output: {output}");
    assert_eq!(blocking_call.count()?, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial(mcp_unbounded_wait_time)]
async fn omitted_and_positive_code_mode_timeouts_remain_finite() -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    for (name, configured_timeout, effective_timeout) in [
        ("code-mode-omitted", None, DEFAULT_TOOL_TIMEOUT),
        (
            "code-mode-positive",
            Some(Duration::from_secs(60)),
            Duration::from_secs(60),
        ),
    ] {
        let server = responses::start_mock_server().await;
        let fixture = build_fixture(&server, configured_timeout, DispatchPath::CodeMode).await?;
        let temp_dir = tempfile::tempdir()?;
        let blocking_call = BlockingCall::new(&temp_dir, name);
        let call_id = format!("call-{name}");
        let cell_id = start_code_mode_call(&server, &fixture, &call_id, &blocking_call).await?;
        let request_count = server
            .received_requests()
            .await
            .expect("mock server should expose received requests")
            .len();

        tokio::time::pause();
        tokio::time::advance(effective_timeout + Duration::from_secs(1)).await;
        tokio::time::resume();
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("mock server should expose received requests")
                .len(),
            request_count
        );
        blocking_call.release()?;

        let output = collect_code_mode_cell(&server, &fixture, &cell_id).await?;
        assert!(
            output.contains("timed out awaiting tools/call"),
            "{name} timeout lost its deadline-specific error: {output}"
        );
        assert_eq!(blocking_call.count()?, 1);
    }
    Ok(())
}
