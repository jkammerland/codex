#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use codex_features::Feature;
use core_test_support::PathBufExt;
use core_test_support::fs_wait;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

fn wait_for_file_source(path: &Path) -> Result<String> {
    let quoted_path = shlex::try_join([path.to_string_lossy().as_ref()])?;
    let command = format!("if [ -f {quoted_path} ]; then printf ready; fi");
    Ok(format!(
        r#"while ((await tools.exec_command({{ cmd: {command:?} }})).output !== "ready") {{
}}"#
    ))
}

#[cfg_attr(windows, ignore = "no exec_command on Windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_cell_dispatches_nested_tool_while_model_turn_is_idle() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(move |config| {
        let _ = config.features.enable(Feature::CodeMode);
    });
    let test = builder.build(&server).await?;
    let completion_gate = test.workspace_path("idle-cell-completion.ready");
    let done_marker = test.workspace_path("idle-cell-done.txt");
    let completion_wait = wait_for_file_source(&completion_gate)?;
    let done_marker_quoted = shlex::try_join([done_marker.to_string_lossy().as_ref()])?;
    let done_command = format!("printf done > {done_marker_quoted}");
    let code = format!(
        r#"
text("cell waiting");
yield_control();
{completion_wait}
await tools.exec_command({{ cmd: {done_command:?} }});
text("cell done");
"#
    );

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("call-1", "exec", &code),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "cell is waiting"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("start a cell that will outlive this turn")
        .await?;

    fs::write(&completion_gate, "ready")?;
    fs_wait::wait_for_path_exists(&done_marker, Duration::from_secs(5)).await?;

    Ok(())
}

#[cfg_attr(windows, ignore = "no exec_command on Windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_cell_keeps_its_originating_turn_environment() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_config(move |config| {
        let _ = config.features.enable(Feature::CodeMode);
    });
    let test = builder.build(&server).await?;
    let first_cwd = test.workspace_path("first-turn");
    let second_cwd = test.workspace_path("second-turn");
    fs::create_dir_all(&first_cwd)?;
    fs::create_dir_all(&second_cwd)?;
    let completion_gate = test.workspace_path("cross-turn-completion.ready");
    let observed_cwd = test.workspace_path("cross-turn-observed-cwd.txt");
    let completion_wait = wait_for_file_source(&completion_gate)?;
    let observed_cwd_quoted = shlex::try_join([observed_cwd.to_string_lossy().as_ref()])?;
    let record_cwd = format!("pwd > {observed_cwd_quoted}");
    let code = format!(
        r#"
text("cell waiting");
yield_control();
{completion_wait}
await tools.exec_command({{ cmd: {record_cwd:?} }});
text("cell done");
"#
    );

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("call-1", "exec", &code),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "cell is waiting"),
            ev_completed("resp-2"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-2", "second turn complete"),
            ev_completed("resp-3"),
        ]),
    )
    .await;

    test.submit_turn_with_environments(
        "start a cell that will outlive this turn",
        Some(vec![local(first_cwd.abs())]),
    )
    .await?;
    test.submit_turn_with_environments(
        "run an unrelated turn in another environment",
        Some(vec![local(second_cwd.abs())]),
    )
    .await?;

    fs::write(&completion_gate, "ready")?;
    fs_wait::wait_for_path_exists(&observed_cwd, Duration::from_secs(5)).await?;
    assert_eq!(
        fs::read_to_string(observed_cwd)?.trim(),
        first_cwd.to_string_lossy()
    );

    Ok(())
}
