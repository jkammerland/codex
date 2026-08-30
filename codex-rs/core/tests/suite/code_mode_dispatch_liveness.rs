#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use codex_features::Feature;
use core_test_support::PathBufExt;
use core_test_support::fs_wait;
use core_test_support::hooks::trust_discovered_hooks;
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

fn write_stale_nested_tool_hook(home: &Path, command_marker: &str, context: &str) -> Result<()> {
    let script_path = home.join("stale_nested_tool_hook.py");
    let script = format!(
        r#"import json
import sys

payload = json.load(sys.stdin)
if {command_marker:?} in payload.get("tool_input", {{}}).get("command", ""):
    print(json.dumps({{
        "hookSpecificOutput": {{
            "hookEventName": "PreToolUse",
            "additionalContext": {context:?}
        }}
    }}))
"#
    );
    let hooks = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", script_path.display()),
                }]
            }]
        }
    });
    fs::write(script_path, script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
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
    const STALE_HOOK_CONTEXT: &str = "context from the originating code-mode turn";
    const STALE_COMMAND_MARKER: &str = "cross-turn-observed-cwd";
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_stale_nested_tool_hook(home, STALE_COMMAND_MARKER, STALE_HOOK_CONTEXT)
                .expect("write stale nested tool hook");
        })
        .with_config(move |config| {
            let _ = config.features.enable(Feature::CodeMode);
            trust_discovered_hooks(config);
        });
    let test = builder.build(&server).await?;
    let first_cwd = test.workspace_path("first-turn");
    let second_cwd = test.workspace_path("second-turn");
    fs::create_dir_all(&first_cwd)?;
    fs::create_dir_all(&second_cwd)?;
    let completion_gate = test.workspace_path("cross-turn-completion.ready");
    let first_cell_progressed = test.workspace_path("first-cell-progressed.txt");
    let second_cell_started = test.workspace_path("second-cell-started.txt");
    let second_completion_gate = test.workspace_path("second-cell-completion.ready");
    let observed_cwd = test.workspace_path("cross-turn-observed-cwd.txt");
    let completion_wait = wait_for_file_source(&completion_gate)?;
    let second_completion_wait = wait_for_file_source(&second_completion_gate)?;
    let observed_cwd_quoted = shlex::try_join([observed_cwd.to_string_lossy().as_ref()])?;
    let first_cell_progressed_quoted =
        shlex::try_join([first_cell_progressed.to_string_lossy().as_ref()])?;
    let second_cell_started_quoted =
        shlex::try_join([second_cell_started.to_string_lossy().as_ref()])?;
    let record_cwd =
        format!("pwd > {observed_cwd_quoted}; printf done > {first_cell_progressed_quoted}");
    let code = format!(
        r#"
text("cell waiting");
yield_control();
{completion_wait}
notify("notice from the originating cell");
await tools.exec_command({{ cmd: {record_cwd:?} }});
text("cell done");
"#
    );
    let second_code = format!(
        r#"
await tools.exec_command({{ cmd: {:?} }});
{second_completion_wait}
text("second cell done");
"#,
        format!("printf started > {second_cell_started_quoted}")
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
            ev_response_created("resp-3"),
            ev_custom_tool_call("call-2", "exec", &second_code),
            ev_completed("resp-3"),
        ]),
    )
    .await;
    let second_followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-2", "second turn complete"),
            ev_completed("resp-4"),
        ]),
    )
    .await;

    test.submit_turn_with_environments(
        "start a cell that will outlive this turn",
        Some(vec![local(first_cwd.abs())]),
    )
    .await?;
    let second_turn = test.submit_turn_with_environments(
        "run an unrelated turn in another environment",
        Some(vec![local(second_cwd.abs())]),
    );
    tokio::pin!(second_turn);
    tokio::select! {
        result = &mut second_turn => {
            result?;
            anyhow::bail!("second turn completed before its code cell was released");
        }
        result = fs_wait::wait_for_path_exists(&second_cell_started, Duration::from_secs(5)) => {
            result?;
        }
    }

    fs::write(&completion_gate, "ready")?;
    fs_wait::wait_for_path_exists(&first_cell_progressed, Duration::from_secs(5)).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(&second_completion_gate, "ready")?;
    second_turn.await?;
    fs_wait::wait_for_path_exists(&observed_cwd, Duration::from_secs(5)).await?;
    assert_eq!(
        fs::read_to_string(observed_cwd)?.trim(),
        first_cwd.to_string_lossy()
    );
    let second_request = second_followup.single_request();
    assert!(
        !second_request
            .inputs_of_type("custom_tool_call_output")
            .iter()
            .any(|item| {
                item.get("output")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|output| output.contains("notice from the originating cell"))
            }),
        "a resumed cell must not inject its notification into another active turn"
    );
    assert!(
        !second_request
            .message_input_texts("developer")
            .contains(&STALE_HOOK_CONTEXT.to_string()),
        "a resumed cell must not inject tool-hook context into another active turn"
    );

    Ok(())
}
