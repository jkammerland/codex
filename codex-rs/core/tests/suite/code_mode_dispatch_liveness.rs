#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
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
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;

fn wait_for_file_source(path: &Path) -> Result<String> {
    let quoted_path = shlex::try_join([path.to_string_lossy().as_ref()])?;
    let command = format!("if [ -f {quoted_path} ]; then printf ready; fi");
    Ok(format!(
        r#"while ((await tools.exec_command({{ cmd: {command:?} }})).output !== "ready") {{
}}"#
    ))
}

fn write_stale_nested_tool_hook(
    home: &Path,
    command_marker: &str,
    pre_context: &str,
    post_context: &str,
    ran_marker: &Path,
) -> Result<()> {
    let script_path = home.join("stale_nested_tool_hook.py");
    let script = format!(
        r#"import json
import sys

payload = json.load(sys.stdin)
if {command_marker:?} in payload.get("tool_input", {{}}).get("command", ""):
    event_name = payload.get("hook_event_name")
    with open({ran_marker:?}, "a", encoding="utf-8") as marker:
        marker.write(event_name + "\n")
    context = {pre_context:?} if event_name == "PreToolUse" else {post_context:?}
    hook_output = {{"hookEventName": event_name}}
    if context:
        hook_output["additionalContext"] = context
    print(json.dumps({{
        "hookSpecificOutput": hook_output
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
            }],
            "PostToolUse": [{
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
async fn active_code_mode_hook_context_is_utf8_safely_bounded() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const COMMAND_MARKER: &str = "active-code-mode-hook-bound";
    let context = format!("active hook marker{}tail", "🦀".repeat(2_200));
    let context_for_hook = context.clone();
    let hook_marker_dir = tempfile::tempdir()?;
    let hook_ran_marker = hook_marker_dir.path().join("active-hook-ran.txt");
    let hook_ran_marker_for_builder = hook_ran_marker.clone();
    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            write_stale_nested_tool_hook(
                home,
                COMMAND_MARKER,
                &context_for_hook,
                "",
                &hook_ran_marker_for_builder,
            )
            .expect("write active nested tool hook");
        })
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            trust_discovered_hooks(config);
        });
    let test = builder.build(&server).await?;
    let code = format!(
        r#"
await tools.exec_command({{ cmd: "printf {COMMAND_MARKER}" }});
text("done");
"#
    );
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-active-hook"),
            ev_custom_tool_call("call-active-hook", "exec", &code),
            ev_completed("resp-active-hook"),
        ]),
    )
    .await;
    let followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-active-hook", "done"),
            ev_completed("resp-active-hook-complete"),
        ]),
    )
    .await;

    test.submit_turn("run a nested tool with oversized hook context")
        .await?;

    let request = followup.single_request();
    let bounded = request
        .message_input_texts("developer")
        .into_iter()
        .find(|text| text.starts_with("active hook marker"))
        .expect("active nested hook should inject additional context");
    assert!(bounded.len() <= 4 * 1024);
    assert!(bounded.ends_with("\n[code-mode input truncated]"));
    assert!(!bounded.ends_with("tail"));
    assert!(request.has_content_kinds(&["hooks.additional_context"]));
    assert_eq!(
        fs::read_to_string(hook_ran_marker)?
            .lines()
            .collect::<Vec<_>>(),
        vec!["PreToolUse", "PostToolUse"]
    );

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
    const STALE_POST_HOOK_CONTEXT: &str = "post context from the originating code-mode turn";
    const STALE_COMMAND_MARKER: &str = "cross-turn-observed-cwd";
    let hook_marker_dir = tempfile::tempdir()?;
    let hook_ran_marker = hook_marker_dir.path().join("stale-hook-ran.txt");
    let hook_ran_marker_for_builder = hook_ran_marker.clone();
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            write_stale_nested_tool_hook(
                home,
                STALE_COMMAND_MARKER,
                STALE_HOOK_CONTEXT,
                STALE_POST_HOOK_CONTEXT,
                &hook_ran_marker_for_builder,
            )
            .expect("write stale nested tool hooks");
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
    let first_followup = responses::mount_sse_once(
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
    let first_request = first_followup.single_request();
    let first_output = first_request.custom_tool_call_output("call-1");
    let first_output = match &first_output["output"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>(),
        output => panic!("initial cell output should be text or content items: {output:?}"),
    };
    let cell_id = first_output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Script running with cell ID "))
        .expect("initial cell output should identify the yielded cell")
        .to_string();
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
        fs::canonicalize(fs::read_to_string(observed_cwd)?.trim())?,
        fs::canonicalize(first_cwd.abs())?
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
            .iter()
            .any(|text| text == STALE_HOOK_CONTEXT || text == STALE_POST_HOOK_CONTEXT),
        "a resumed cell must not inject tool-hook context into another active turn"
    );

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-5"),
            responses::ev_function_call(
                "call-3",
                "wait",
                &serde_json::to_string(&serde_json::json!({ "cell_id": cell_id }))?,
            ),
            ev_completed("resp-5"),
        ]),
    )
    .await;
    let resumed_followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-3", "resumed cell observed"),
            ev_completed("resp-6"),
        ]),
    )
    .await;

    test.submit_turn("observe the completed originating cell")
        .await?;

    let resumed_request = resumed_followup.single_request();
    assert!(
        resumed_request
            .inputs_of_type("custom_tool_call_output")
            .iter()
            .any(|item| {
                item.get("output")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|output| output.contains("notice from the originating cell"))
            }),
        "the next observation should surface the originating cell notification"
    );
    assert!(
        resumed_request
            .message_input_texts("developer")
            .iter()
            .any(|text| text == STALE_HOOK_CONTEXT),
        "the next observation should surface deferred PreToolUse context"
    );
    assert!(
        resumed_request
            .message_input_texts("developer")
            .iter()
            .any(|text| text == STALE_POST_HOOK_CONTEXT),
        "the next observation should surface deferred PostToolUse context"
    );
    assert!(
        resumed_request.has_content_kinds(&["hooks.additional_context"]),
        "deferred hook context should remain a typed contextual fragment"
    );
    assert_eq!(
        fs::read_to_string(&hook_ran_marker)?
            .lines()
            .collect::<Vec<_>>(),
        vec!["PreToolUse", "PostToolUse"],
        "positive control should prove both resumed hooks ran"
    );

    Ok(())
}

#[cfg_attr(windows, ignore = "no exec_command on Windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_inputs_wake_an_indefinitely_live_cell_observation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const COMMAND_MARKER: &str = "live-cell-deferred-input";
    const PRE_CONTEXT: &str = "";
    const POST_CONTEXT: &str = "live cell post-hook context";
    const NOTIFICATION: &str = "live cell notification";
    let hook_marker_dir = tempfile::tempdir()?;
    let hook_ran_marker = hook_marker_dir.path().join("live-cell-hook-ran.txt");
    let hook_ran_marker_for_builder = hook_ran_marker.clone();
    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            write_stale_nested_tool_hook(
                home,
                COMMAND_MARKER,
                PRE_CONTEXT,
                POST_CONTEXT,
                &hook_ran_marker_for_builder,
            )
            .expect("write live-cell nested tool hooks");
        })
        .with_config(|config| {
            let _ = config.features.enable(Feature::CodeMode);
            trust_discovered_hooks(config);
        });
    let test = builder.build(&server).await?;
    let hook_release_gate = test.workspace_path("live-cell-deferred-input.ready");
    let hook_release_gate_quoted = shlex::try_join([hook_release_gate.to_string_lossy().as_ref()])?;
    let blocking_command = format!(
        "while [ ! -f {hook_release_gate_quoted} ]; do sleep 0.01; done; printf {COMMAND_MARKER}"
    );
    let notification_command_started = test.workspace_path("notification-command-started.txt");
    let notification_command_started_quoted =
        shlex::try_join([notification_command_started.to_string_lossy().as_ref()])?;
    let notification_release_gate = test.workspace_path("notification-release.ready");
    let notification_release_gate_quoted =
        shlex::try_join([notification_release_gate.to_string_lossy().as_ref()])?;
    let notification_command = format!(
        "printf started > {notification_command_started_quoted}; \
         while [ ! -f {notification_release_gate_quoted} ]; do sleep 0.01; done"
    );
    let code = format!(
        r#"
yield_control();
await tools.exec_command({{ cmd: {blocking_command:?}, yield_time_ms: 60_000 }});
await tools.exec_command({{ cmd: {notification_command:?}, yield_time_ms: 60_000 }});
notify({NOTIFICATION:?});
await new Promise(() => {{}});
"#
    );

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-live-input-exec"),
            ev_custom_tool_call("call-live-input-exec", "exec", &code),
            ev_completed("resp-live-input-exec"),
        ]),
    )
    .await;
    let initial_followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-live-input-waiting", "cell is waiting"),
            ev_completed("resp-live-input-waiting"),
        ]),
    )
    .await;
    test.submit_turn("start an indefinitely live cell").await?;

    let initial_output = initial_followup
        .single_request()
        .custom_tool_call_output("call-live-input-exec");
    let initial_output = match &initial_output["output"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
            .collect::<String>(),
        output => panic!("initial live-cell output should be text or items: {output:?}"),
    };
    let cell_id = initial_output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Script running with cell ID "))
        .expect("initial live-cell output should identify the yielded cell")
        .to_string();
    fs_wait::wait_for_path_exists(&hook_ran_marker, Duration::from_secs(5)).await?;
    assert_eq!(
        fs::read_to_string(&hook_ran_marker)?
            .lines()
            .collect::<Vec<_>>(),
        vec!["PreToolUse"],
        "only the marker-only pre hook should run before wait starts"
    );

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-live-input-wait"),
            responses::ev_function_call(
                "call-live-input-wait",
                "wait",
                &serde_json::to_string(&serde_json::json!({
                    "cell_id": cell_id.clone(),
                    "yield_time_ms": 1,
                }))?,
            ),
            ev_completed("resp-live-input-wait"),
        ]),
    )
    .await;
    let resumed_followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-live-input-observed", "input observed"),
            ev_completed("resp-live-input-observed"),
        ]),
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "wait for deferred live-cell input".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RawResponseItem(raw) => match &raw.item {
            ResponseItem::FunctionCall { call_id, .. } if call_id == "call-live-input-wait" => {
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    fs::write(&hook_release_gate, "ready")?;
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }),
    )
    .await?;

    let resumed_request = resumed_followup.single_request();
    assert!(
        resumed_request
            .message_input_texts("developer")
            .iter()
            .any(|text| text == POST_CONTEXT),
        "deferred hook context should wake and surface from a still-live cell"
    );
    assert!(
        resumed_request.has_content_kinds(&["hooks.additional_context"]),
        "live-cell hook context should remain a typed contextual fragment"
    );
    assert!(
        fs::read_to_string(&hook_ran_marker)?
            .lines()
            .any(|event| event == "PostToolUse"),
        "positive control should prove the resumed post hook ran"
    );

    fs_wait::wait_for_path_exists(&notification_command_started, Duration::from_secs(5)).await?;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-live-notification-wait"),
            responses::ev_function_call(
                "call-live-notification-wait",
                "wait",
                &serde_json::to_string(&serde_json::json!({
                    "cell_id": cell_id,
                    "yield_time_ms": 1,
                }))?,
            ),
            ev_completed("resp-live-notification-wait"),
        ]),
    )
    .await;
    let notification_followup = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-live-notification-observed", "notification observed"),
            ev_completed("resp-live-notification-observed"),
        ]),
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "wait for the deferred live-cell notification".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RawResponseItem(raw) => match &raw.item {
            ResponseItem::FunctionCall { call_id, .. }
                if call_id == "call-live-notification-wait" =>
            {
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    fs::write(&notification_release_gate, "ready")?;
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        }),
    )
    .await?;

    let notification_request = notification_followup.single_request();
    assert!(
        notification_request
            .inputs_of_type("custom_tool_call_output")
            .iter()
            .any(|item| {
                item.get("output")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|output| output.contains(NOTIFICATION))
            }),
        "deferred notification should wake and surface from a still-live cell"
    );
    Ok(())
}
