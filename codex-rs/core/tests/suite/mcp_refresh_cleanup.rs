use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_protocol::protocol::Op;
use core_test_support::fs_wait::wait_for_path_exists;
use core_test_support::process::process_is_alive;
use core_test_support::process::wait_for_pid_file;
use core_test_support::process::wait_for_process_exit;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_keeps_superseded_mcp_server_alive_for_in_flight_calls() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let pid_file = temp_dir.path().join("mcp.pid");
    let call_started_file = temp_dir.path().join("mcp-call.started");
    let call_release_file = temp_dir.path().join("mcp-call.release");
    let pid_file_for_config = pid_file.clone();
    let command = stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "refresh_cleanup".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command,
                        args: Vec::new(),
                        env: Some(HashMap::from([(
                            "MCP_TEST_PID_FILE".to_string(),
                            pid_file_for_config.to_string_lossy().into_owned(),
                        )])),
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: Some(Duration::ZERO),
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
        })
        .build(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, "refresh_cleanup").await?;

    let superseded_pid = wait_for_pid_file(&pid_file).await?;
    assert!(process_is_alive(&superseded_pid)?);

    let long_call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let call_started_file = call_started_file.clone();
        let call_release_file = call_release_file.clone();
        async move {
            codex
                .call_mcp_tool(
                    "refresh_cleanup",
                    "sync",
                    Some(serde_json::json!({
                        "started_file": call_started_file,
                        "release_file": call_release_file,
                    })),
                    /*meta*/ None,
                )
                .await
        }
    });
    wait_for_path_exists(&call_started_file, Duration::from_secs(5)).await?;
    fs::remove_file(&pid_file)?;

    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    fixture.codex.submit(Op::RefreshMcpServers).await?;
    fixture.submit_turn("refresh MCP servers").await?;

    let replacement_pid = wait_for_pid_file(&pid_file).await?;
    assert_ne!(replacement_pid, superseded_pid);
    assert!(process_is_alive(&superseded_pid)?);
    fs::write(&call_release_file, "release")?;
    long_call.await??;
    wait_for_process_exit(&superseded_pid).await?;
    assert!(process_is_alive(&replacement_pid)?);

    fixture.codex.shutdown_and_wait().await?;
    wait_for_process_exit(&replacement_pid).await
}
