use std::path::Path;

use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use wiremock::MockServer;

async fn start_code_mode_test(server: &MockServer) -> Result<TestCodex> {
    test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        })
        .build(server)
        .await
}

async fn run_code_cell(
    server: &MockServer,
    test: &TestCodex,
    index: usize,
    source: &str,
) -> Result<ResponseMock> {
    let response_id = format!("resp-live-session-{index}");
    let completion_id = format!("resp-live-session-complete-{index}");
    let message_id = format!("msg-live-session-{index}");
    let call_id = format!("call-live-session-{index}");
    responses::mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_response_created(&response_id),
            responses::ev_custom_tool_call(&call_id, "exec", source),
            responses::ev_completed(&response_id),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_assistant_message(&message_id, "done"),
            responses::ev_completed(&completion_id),
        ]),
    )
    .await;

    test.submit_turn(&format!("run code cell {index}")).await?;
    Ok(completion)
}

fn code_cell_output(request: &ResponsesRequest, call_id: &str) -> String {
    match request.custom_tool_call_output(call_id).get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<String>(),
        output => panic!("unexpected code cell output: {output:?}"),
    }
}

fn recoverable_session_ids(output: &str) -> Vec<i32> {
    let Some(metadata) = output
        .lines()
        .find(|line| line.starts_with("{\"live_sessions\":"))
    else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(metadata)
        .expect("live-session metadata should be valid JSON")
        .get("live_sessions")
        .and_then(Value::as_array)
        .expect("live-session metadata should contain an array")
        .iter()
        .map(|session| {
            session
                .get("session_id")
                .and_then(Value::as_i64)
                .and_then(|session_id| i32::try_from(session_id).ok())
                .expect("live-session metadata should contain an i32 session ID")
        })
        .collect()
}

fn wait_for_release_command(release_file: &Path) -> Result<String> {
    let release_file = shlex::try_join([release_file.to_string_lossy().as_ref()])?;
    Ok(format!(
        "while [ ! -f {release_file} ]; do sleep 0.01; done; printf released"
    ))
}

fn nested_exec_source(command: &str, emitted_expression: &str) -> String {
    let command = serde_json::to_string(command).expect("command should serialize");
    format!(
        r#"
const result = await tools.exec_command({{
  cmd: {command},
  yield_time_ms: 250,
}});
text({emitted_expression});
"#
    )
}

async fn release_and_wait(
    server: &MockServer,
    test: &TestCodex,
    release_file: &Path,
    session_id: i32,
    index: usize,
) -> Result<String> {
    std::fs::write(release_file, "release")?;
    let source = format!(
        r#"
const result = await tools.wait_process({{ session_id: {session_id} }});
text(JSON.stringify(result));
"#
    );
    let completion = run_code_cell(server, test, index, &source).await?;
    let request = completion.single_request();
    Ok(code_cell_output(
        &request,
        &format!("call-live-session-{index}"),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_surfaces_live_session_when_result_output_is_emitted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_code_mode_test(&server).await?;
    let temp_dir = tempfile::tempdir()?;
    let release_file = temp_dir.path().join("projected-away.release");
    let command = wait_for_release_command(&release_file)?;
    let completion = run_code_cell(
        &server,
        &test,
        1,
        &nested_exec_source(&command, "result.output"),
    )
    .await?;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");
    let session_ids = recoverable_session_ids(&output);
    assert_eq!(
        session_ids.len(),
        1,
        "unexpected code cell output: {output}"
    );

    let recovered = release_and_wait(&server, &test, &release_file, session_ids[0], 2).await?;
    assert!(recovered.contains("released"));
    assert_eq!(recoverable_session_ids(&recovered), Vec::<i32>::new());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_preserves_live_session_in_complete_result() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_code_mode_test(&server).await?;
    let temp_dir = tempfile::tempdir()?;
    let release_file = temp_dir.path().join("complete-result.release");
    let command = wait_for_release_command(&release_file)?;
    let completion = run_code_cell(
        &server,
        &test,
        1,
        &nested_exec_source(&command, "JSON.stringify(result)"),
    )
    .await?;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");
    let session_ids = recoverable_session_ids(&output);
    assert_eq!(
        session_ids.len(),
        1,
        "unexpected code cell output: {output}"
    );
    assert!(output.contains(&format!("\"session_id\":{}", session_ids[0])));

    release_and_wait(&server, &test, &release_file, session_ids[0], 2).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_nested_command_does_not_report_live_session() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_code_mode_test(&server).await?;
    let completion = run_code_cell(
        &server,
        &test,
        1,
        &nested_exec_source("printf completed", "result.output"),
    )
    .await?;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");
    assert!(
        output.contains("completed"),
        "unexpected code cell output: {output}"
    );
    assert_eq!(recoverable_session_ids(&output), Vec::<i32>::new());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumed_session_is_not_reported_as_live() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_code_mode_test(&server).await?;
    let source = r#"
let result = await tools.exec_command({
  cmd: "sleep 1; printf completed",
  yield_time_ms: 250,
});
while (result.session_id !== undefined) {
  result = await tools.wait_process({ session_id: result.session_id });
}
text(JSON.stringify(result));
"#;
    let completion = run_code_cell(&server, &test, 1, source).await?;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");
    assert!(
        output.contains("completed"),
        "unexpected code cell output: {output}"
    );
    assert_eq!(recoverable_session_ids(&output), Vec::<i32>::new());
    Ok(())
}
