use std::path::Path;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use core_test_support::TestTargetOs;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::test_target_os;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
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

async fn start_code_mode_test_with_auto_env(server: &MockServer) -> Result<TestCodex> {
    test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        })
        .build_with_auto_env(server)
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
    wait_for_session(server, test, session_id, index).await
}

async fn wait_for_session(
    server: &MockServer,
    test: &TestCodex,
    session_id: i32,
    index: usize,
) -> Result<String> {
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
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
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
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
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
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
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
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_unconsumed_session_is_reported_and_recovered_exactly_once() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let test = start_code_mode_test_with_auto_env(&server).await?;
    let (projected_command, cell_gate_command) = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => (
            "while [ ! -f producer.release ]; do sleep 0.01; done; printf completed",
            "while [ ! -f cell.release ]; do sleep 0.01; done",
        ),
        TestTargetOs::Windows => (
            "while (!(Test-Path -LiteralPath producer.release)) { Start-Sleep -Milliseconds 10 }; Write-Output completed",
            "while (!(Test-Path -LiteralPath cell.release)) { Start-Sleep -Milliseconds 10 }",
        ),
    };
    let source = format!(
        r#"// @exec: {{"yield_time_ms": 120000}}
const projected = await tools.exec_command({{
  cmd: {projected_command:?},
  yield_time_ms: 250,
}});
// This gate keeps the JS cell open until the harness observes the original producer's End.
let gate = await tools.exec_command({{ cmd: {cell_gate_command:?}, yield_time_ms: 30000 }});
while (gate.session_id !== undefined) {{
  gate = await tools.wait_process({{ session_id: gate.session_id }});
}}
text(projected.output);
"#
    );
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-completed-unconsumed"),
            responses::ev_custom_tool_call("call-live-session-1", "exec", &source),
            responses::ev_completed("resp-completed-unconsumed"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-completed-unconsumed", "done"),
            responses::ev_completed("resp-completed-unconsumed-done"),
        ]),
    )
    .await;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "complete the producer before the cell returns".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                ..Default::default()
            }),
        )
        .await?;
    let producer = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ExecCommandBegin(event)
            if event.command.iter().any(|part| part.contains("producer.release")) =>
        {
            Some(event.call_id.clone())
        }
        _ => None,
    })
    .await;
    // Reaching the second nested command proves the producer yielded a live session first.
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ExecCommandBegin(event)
            if event.command.iter().any(|part| part.contains("cell.release")))
    })
    .await;
    test.fs()
        .write_file(
            &test.workspace_path_uri("producer.release")?,
            b"release".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ExecCommandEnd(event) if event.call_id == producer)
    })
    .await;
    test.fs()
        .write_file(
            &test.workspace_path_uri("cell.release")?,
            b"release".to_vec(),
            Default::default(),
            /*sandbox*/ None,
        )
        .await?;
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");
    let session_ids = recoverable_session_ids(&output);
    assert_eq!(
        session_ids.len(),
        1,
        "completed but unconsumed session should remain recoverable: {output}"
    );

    let recovered = wait_for_session(&server, &test, session_ids[0], 2).await?;
    assert!(
        recovered.contains("completed") && recovered.contains("\"exit_code\":0"),
        "first wait should retrieve the completed process exactly once: {recovered}"
    );

    let replay = wait_for_session(&server, &test, session_ids[0], 3).await?;
    assert!(
        replay.contains("Unknown process id") && !replay.contains("\"exit_code\":0"),
        "a consumed session must not replay its final result: {replay}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_code_mode_output_caps_text_without_losing_live_session_metadata() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX sleep command fixture");
    skip_if_no_network!(Ok(()));

    const OUTER_MAX_OUTPUT_TOKENS: usize = 128;
    let server = responses::start_mock_server().await;
    let test = start_code_mode_test(&server).await?;
    let source = format!(
        r#"// @exec: {{"max_output_tokens": {OUTER_MAX_OUTPUT_TOKENS}}}
const projected = await tools.exec_command({{
  cmd: "sleep 1; printf completed",
  yield_time_ms: 250,
}});
text("payload-".repeat(1_000));
"#
    );
    let completion = run_code_cell(&server, &test, 1, &source).await?;
    let output = code_cell_output(&completion.single_request(), "call-live-session-1");

    assert!(
        output.starts_with("Script completed"),
        "final truncation should preserve the script status: {output}"
    );
    let session_ids = recoverable_session_ids(&output);
    assert_eq!(
        session_ids.len(),
        1,
        "final truncation should preserve recoverable-session metadata: {output}"
    );
    let output_tokens = approx_token_count(&output);
    assert!(
        output_tokens <= OUTER_MAX_OUTPUT_TOKENS,
        "final output used {output_tokens} tokens with a {OUTER_MAX_OUTPUT_TOKENS}-token cap"
    );

    let recovered = wait_for_session(&server, &test, session_ids[0], 2).await?;
    assert!(recovered.contains("completed"));
    Ok(())
}
