#![cfg(not(windows))]
//
// Running these tests with the patched zsh fork:
//
// The suite resolves the shared test-only zsh DotSlash file at
// `app-server/tests/suite/zsh` via DotSlash on first use, so `dotslash` and
// network access are required the first time the artifact is fetched.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_command_execution_sse_response;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadArchivedNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn turn_start_shell_zsh_fork_executes_command_v2() -> Result<()> {
    // TODO(anp): Remove after zsh-fork fixtures can run in the selected remote environment.
    skip_if_remote!(
        Ok(()),
        "zsh-fork fixtures use host-local zsh and workspace paths"
    );
    skip_if_no_network!(Ok(()));

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let release_marker = workspace.join("interrupt-release");

    let Some(zsh_path) = find_test_zsh_path()? else {
        eprintln!("skipping zsh fork test: no zsh executable found");
        return Ok(());
    };
    eprintln!("using zsh path for zsh-fork test: {}", zsh_path.display());

    // Keep the shell command in flight until we interrupt it. A fast command
    // like `echo hi` can finish before the interrupt arrives on faster runners,
    // which turns this into a test for post-command follow-up behavior instead
    // of interrupting an active zsh-fork command.
    let release_marker_escaped = release_marker.to_string_lossy().replace('\'', r#"'\''"#);
    let wait_for_interrupt =
        format!("while [ ! -f '{release_marker_escaped}' ]; do sleep 0.01; done");
    let response = create_command_execution_sse_response(
        vec!["/bin/sh".to_string(), "-c".to_string(), wait_for_interrupt],
        /*workdir*/ None,
        Some(5000),
        "call-zsh-fork",
    )?;
    let no_op_response = responses::sse(vec![
        responses::ev_response_created("resp-2"),
        responses::ev_completed("resp-2"),
    ]);
    // Interrupting after the shell item starts can race with the follow-up
    // model request that reports the aborted tool call. This test only cares
    // that zsh-fork launches the expected command, so allow one extra no-op
    // `/responses` POST instead of asserting an exact request count.
    let server =
        create_mock_responses_server_sequence_unchecked(vec![response, no_op_response]).await;
    create_config_toml(
        &codex_home,
        &server.uri(),
        "never",
        &BTreeMap::from([
            (Feature::ShellZshFork, true),
            (Feature::ShellSnapshot, false),
        ]),
    )?;

    let mut mcp = create_zsh_test_mcp_process(&codex_home, &workspace, &zsh_path).await?;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(start_id)).await??;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "run echo hi".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(workspace.clone()),
            approval_policy: Some(codex_app_server_protocol::AskForApproval::Never),
            sandbox_policy: Some(codex_app_server_protocol::SandboxPolicy::DangerFullAccess),
            model: Some("mock-model".to_string()),
            effort: Some(codex_protocol::openai_models::ReasoningEffort::Medium),
            summary: Some(codex_protocol::config_types::ReasoningSummary::Auto),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;

    let started_command_execution = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let started_notif = mcp
                .read_stream_until_notification_message("item/started")
                .await?;
            let started: ItemStartedNotification =
                serde_json::from_value(started_notif.params.clone().expect("item/started params"))?;
            if let ThreadItem::CommandExecution { .. } = started.item {
                return Ok::<ThreadItem, anyhow::Error>(started.item);
            }
        }
    })
    .await??;
    let ThreadItem::CommandExecution {
        id,
        status,
        command,
        cwd,
        ..
    } = started_command_execution
    else {
        unreachable!("loop ensures we break on command execution items");
    };
    assert_eq!(id, "call-zsh-fork");
    assert_eq!(status, CommandExecutionStatus::InProgress);
    assert!(command.starts_with(&command_packaged_zsh_path(&codex_home).display().to_string()));
    assert!(command.contains("/bin/sh -c"));
    assert!(command.contains("sleep 0.01"));
    assert!(command.contains(&release_marker.display().to_string()));
    assert_eq!(cwd.as_str(), workspace.to_string_lossy().as_ref());

    mcp.interrupt_turn_and_wait_for_aborted(thread.id, turn.id, DEFAULT_READ_TIMEOUT)
        .await?;

    Ok(())
}

#[tokio::test]
async fn turn_start_shell_zsh_fork_exec_approval_decline_v2() -> Result<()> {
    // TODO(anp): Remove after zsh-fork fixtures can run in the selected remote environment.
    skip_if_remote!(
        Ok(()),
        "zsh-fork fixtures use host-local zsh and workspace paths"
    );
    skip_if_no_network!(Ok(()));

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let Some(zsh_path) = find_test_zsh_path()? else {
        eprintln!("skipping zsh fork decline test: no zsh executable found");
        return Ok(());
    };
    eprintln!("using zsh path for zsh-fork test: {}", zsh_path.display());

    let responses = vec![
        create_escalated_command_execution_sse_response(
            vec![
                "python3".to_string(),
                "-c".to_string(),
                "print(42)".to_string(),
            ],
            /*workdir*/ None,
            Some(5000),
            "call-zsh-fork-decline",
        )?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    create_config_toml(
        &codex_home,
        &server.uri(),
        "on-request",
        &BTreeMap::from([
            (Feature::ShellZshFork, true),
            (Feature::ShellSnapshot, false),
        ]),
    )?;

    let mut mcp = create_zsh_test_mcp_process(&codex_home, &workspace, &zsh_path).await?;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(start_id)).await??;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "run python".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(workspace.clone()),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, params } = server_req else {
        panic!("expected CommandExecutionRequestApproval request");
    };
    assert_eq!(params.item_id, "call-zsh-fork-decline");
    assert_eq!(params.thread_id, thread.id);

    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Decline,
        })?,
    )
    .await?;

    let completed_command_execution = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed_notif = mcp
                .read_stream_until_notification_message("item/completed")
                .await?;
            let completed: ItemCompletedNotification = serde_json::from_value(
                completed_notif
                    .params
                    .clone()
                    .expect("item/completed params"),
            )?;
            if let ThreadItem::CommandExecution { .. } = completed.item {
                return Ok::<ThreadItem, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    let ThreadItem::CommandExecution {
        id,
        status,
        exit_code,
        aggregated_output,
        ..
    } = completed_command_execution
    else {
        unreachable!("loop ensures we break on command execution items");
    };
    assert_eq!(id, "call-zsh-fork-decline");
    assert_eq!(status, CommandExecutionStatus::Declined);
    assert!(exit_code.is_none());
    assert!(aggregated_output.is_none());

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    Ok(())
}

#[tokio::test]
async fn turn_start_shell_zsh_fork_exec_approval_cancel_v2() -> Result<()> {
    // TODO(anp): Remove after zsh-fork fixtures can run in the selected remote environment.
    skip_if_remote!(
        Ok(()),
        "zsh-fork fixtures use host-local zsh and workspace paths"
    );
    skip_if_no_network!(Ok(()));

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let Some(zsh_path) = find_test_zsh_path()? else {
        eprintln!("skipping zsh fork cancel test: no zsh executable found");
        return Ok(());
    };
    eprintln!("using zsh path for zsh-fork test: {}", zsh_path.display());

    let responses = vec![create_escalated_command_execution_sse_response(
        vec![
            "python3".to_string(),
            "-c".to_string(),
            "print(42)".to_string(),
        ],
        /*workdir*/ None,
        Some(5000),
        "call-zsh-fork-cancel",
    )?];
    let server = create_mock_responses_server_sequence(responses).await;
    create_config_toml(
        &codex_home,
        &server.uri(),
        "on-request",
        &BTreeMap::from([
            (Feature::ShellZshFork, true),
            (Feature::ShellSnapshot, false),
        ]),
    )?;

    let mut mcp = create_zsh_test_mcp_process(&codex_home, &workspace, &zsh_path).await?;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(start_id)).await??;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "run python".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(workspace.clone()),
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, params } = server_req else {
        panic!("expected CommandExecutionRequestApproval request");
    };
    assert_eq!(params.item_id, "call-zsh-fork-cancel");
    assert_eq!(params.thread_id, thread.id.clone());

    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Cancel,
        })?,
    )
    .await?;

    let completed_command_execution = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed_notif = mcp
                .read_stream_until_notification_message("item/completed")
                .await?;
            let completed: ItemCompletedNotification = serde_json::from_value(
                completed_notif
                    .params
                    .clone()
                    .expect("item/completed params"),
            )?;
            if let ThreadItem::CommandExecution { .. } = completed.item {
                return Ok::<ThreadItem, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    let ThreadItem::CommandExecution { id, status, .. } = completed_command_execution else {
        unreachable!("loop ensures we break on command execution items");
    };
    assert_eq!(id, "call-zsh-fork-cancel");
    assert_eq!(status, CommandExecutionStatus::Declined);

    let completed_notif = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let completed: TurnCompletedNotification = serde_json::from_value(
        completed_notif
            .params
            .expect("turn/completed params must be present"),
    )?;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);

    Ok(())
}

#[tokio::test]
async fn turn_start_shell_zsh_fork_late_subcommand_cancel_does_not_restart_parent_v2(
) -> Result<()> {
    // TODO(anp): Remove after zsh-fork fixtures can run in the selected remote environment.
    skip_if_remote!(
        Ok(()),
        "zsh-fork fixtures use host-local zsh and workspace paths"
    );
    skip_if_no_network!(Ok(()));

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let Some(zsh_path) = find_test_zsh_path()? else {
        eprintln!("skipping zsh fork subcommand cancel test: no zsh executable found");
        return Ok(());
    };
    if !supports_exec_wrapper_intercept(&zsh_path) {
        eprintln!(
            "skipping zsh fork subcommand cancel test: zsh does not support EXEC_WRAPPER intercepts ({})",
            zsh_path.display()
        );
        return Ok(());
    }
    eprintln!("using zsh path for zsh-fork test: {}", zsh_path.display());
    let release_marker_path = workspace.join("release-late-subcommand");
    let target_file = workspace.join("keep-after-cancel.txt");
    std::fs::write(&target_file, "keep")?;
    let release_marker = release_marker_path
        .to_string_lossy()
        .replace('\'', r#"'\''"#);
    let target_file_escaped = target_file.to_string_lossy().replace('\'', r#"'\''"#);
    let shell_command = format!(
        "while [[ ! -f '{release_marker}' ]]; do :; done; /bin/rm '{target_file_escaped}'"
    );
    let call_id = "call-zsh-fork-late-subcommand-cancel";
    let tool_call_arguments = serde_json::to_string(&serde_json::json!({
        "cmd": shell_command,
        "workdir": serde_json::Value::Null,
        "yield_time_ms": 100
    }))?;
    let response = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_function_call(
            call_id,
            "exec_command",
            &tool_call_arguments,
        ),
        responses::ev_completed("resp-1"),
    ]);
    let no_op_response = responses::sse(vec![
        responses::ev_response_created("resp-2"),
        responses::ev_completed("resp-2"),
    ]);
    // The parent intentionally yields while its builtin loop is still running.
    // The second response lets the turn finish before the loop releases the
    // intercepted subcommand.
    let server =
        create_mock_responses_server_sequence_unchecked(vec![response, no_op_response]).await;
    create_config_toml(
        &codex_home,
        &server.uri(),
        "on-request",
        &BTreeMap::from([
            (Feature::ShellZshFork, true),
            (Feature::ShellSnapshot, false),
        ]),
    )?;

    let mut mcp = create_zsh_test_mcp_process(&codex_home, &workspace, &zsh_path).await?;

    let start_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(start_id)).await??;

    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "remove the file after the release marker appears".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(workspace.clone()),
            approval_policy: Some(codex_app_server_protocol::AskForApproval::UnlessTrusted),
            // This test is about execve-intercept approval propagation, not
            // workspace sandboxing. Using full access avoids macOS sandbox
            // setup failures that can terminate the parent shell before the
            // delayed subcommand approval is observed.
            sandbox_policy: Some(codex_app_server_protocol::SandboxPolicy::DangerFullAccess),
            model: Some("mock-model".to_string()),
            effort: Some(codex_protocol::openai_models::ReasoningEffort::Medium),
            summary: Some(codex_protocol::config_types::ReasoningSummary::Auto),
            ..Default::default()
        })
        .await?;
    let TurnStartResponse { turn } =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(turn_id)).await??;

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, params } = server_req else {
        panic!("expected parent CommandExecutionRequestApproval request");
    };
    assert_eq!(params.item_id, call_id);
    assert_eq!(params.thread_id, thread.id);
    assert_eq!(params.approval_id, None);
    let approval_command = params
        .command
        .as_deref()
        .expect("parent approval command should be present");
    assert!(
        approval_command.contains(&command_packaged_zsh_path(&codex_home).display().to_string())
    );
    assert!(approval_command.contains("release-late-subcommand"));
    assert!(approval_command.contains("keep-after-cancel.txt"));
    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Accept,
        })?,
    )
    .await?;

    let parent_started_metadata = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let started_notif = mcp
                .read_stream_until_notification_message("item/started")
                .await?;
            let started: ItemStartedNotification =
                serde_json::from_value(started_notif.params.expect("item/started params"))?;
            if let ThreadItem::CommandExecution {
                id,
                command,
                cwd,
                command_actions,
                ..
            } = started.item
                && id == call_id
            {
                return Ok::<_, anyhow::Error>((command, cwd, command_actions));
            }
        }
    })
    .await??;
    let completed_notif = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let completed: TurnCompletedNotification = serde_json::from_value(
        completed_notif
            .params
            .expect("turn/completed params must be present"),
    )?;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.id, turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    mcp.clear_message_buffer();

    std::fs::write(&release_marker_path, "release")?;
    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::CommandExecutionRequestApproval { request_id, params } = server_req else {
        panic!("expected late subcommand CommandExecutionRequestApproval request");
    };
    assert_eq!(params.item_id, call_id);
    assert_eq!(params.thread_id, thread.id);
    assert_eq!(params.turn_id, turn.id);
    assert!(
        params.approval_id.is_some(),
        "the intercepted subcommand must have its own approval id"
    );
    assert!(
        !mcp.pending_notification_methods()
            .iter()
            .any(|method| method == "item/started"),
        "the late subcommand approval must not restart the yielded parent"
    );
    let approval_command = params
        .command
        .as_deref()
        .expect("subcommand approval command should be present");
    assert!(approval_command.contains("/rm "));
    assert!(approval_command.contains(target_file.to_string_lossy().as_ref()));
    let resolved_request_id = request_id.clone();
    mcp.send_response(
        request_id,
        serde_json::to_value(CommandExecutionRequestApprovalResponse {
            decision: CommandExecutionApprovalDecision::Cancel,
        })?,
    )
    .await?;

    let resolved: ServerRequestResolvedNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_notification("serverRequest/resolved"),
    )
    .await??;
    assert_eq!(resolved.thread_id, thread.id);
    assert_eq!(resolved.request_id, resolved_request_id);

    let parent_completion = timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            let completed_notif = mcp
                .read_stream_until_notification_message("item/completed")
                .await?;
            let completed: ItemCompletedNotification = serde_json::from_value(
                completed_notif
                    .params
                    .clone()
                    .expect("item/completed params"),
            )?;
            if let ThreadItem::CommandExecution { id, .. } = &completed.item
                && id == call_id
            {
                return Ok::<ItemCompletedNotification, anyhow::Error>(completed);
            }
        }
    })
    .await??;
    assert_eq!(parent_completion.thread_id, thread.id);
    assert_eq!(parent_completion.turn_id, turn.id);
    let ThreadItem::CommandExecution {
        id,
        command,
        cwd,
        command_actions,
        status,
        ..
    } = parent_completion.item
    else {
        unreachable!("loop ensures we break on parent command execution item");
    };
    assert_eq!(id, call_id);
    assert!(
        matches!(
            status,
            CommandExecutionStatus::Declined | CommandExecutionStatus::Failed
        ),
        "the cancelled late subcommand must terminate the parent command"
    );
    assert_eq!(
        (command, cwd, command_actions),
        parent_started_metadata,
        "the late approval must complete the original parent metadata"
    );

    // Archiving is cleanup-only and happens after the late-cancel lifecycle
    // assertions. It also provides a real thread-shutdown barrier before the
    // final exactly-once check.
    let archive_id = mcp
        .send_thread_archive_request(ThreadArchiveParams {
            thread_id: thread.id.clone(),
        })
        .await?;
    let _: ThreadArchiveResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(archive_id)).await??;
    let archived: ThreadArchivedNotification =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_notification("thread/archived")).await??;
    assert_eq!(archived.thread_id, thread.id);
    assert!(target_file.exists(), "the cancelled rm must not run");
    assert_eq!(
        mcp.pending_notification_methods()
            .into_iter()
            .filter(|method| method == "item/started" || method == "item/completed")
            .count(),
        0,
        "the late approval must not restart or recomplete the yielded parent"
    );

    Ok(())
}

async fn create_zsh_test_mcp_process(
    codex_home: &Path,
    zdotdir: &Path,
    zsh_path: &Path,
) -> Result<TestAppServer> {
    let app_server = create_test_package_app_server(codex_home, zsh_path)?;
    let zdotdir = zdotdir.to_string_lossy().into_owned();
    TestAppServer::builder()
        .with_codex_home(codex_home)
        .with_program(&app_server)
        .with_env_overrides(&[("ZDOTDIR", Some(zdotdir.as_str()))])
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await
}

fn create_test_package_app_server(codex_home: &Path, zsh_path: &Path) -> Result<PathBuf> {
    let package_dir = codex_home.join("test-package");
    let bin_dir = package_dir.join("bin");
    let package_zsh_path = packaged_zsh_path(codex_home);
    let Some(zsh_bin_dir) = package_zsh_path.parent() else {
        anyhow::bail!("packaged zsh path should have parent");
    };
    std::fs::create_dir_all(&bin_dir)?;
    std::fs::create_dir_all(zsh_bin_dir)?;
    std::fs::write(package_dir.join("codex-package.json"), "{}")?;

    let app_server = bin_dir.join("codex-app-server");
    copy_with_permissions(
        &codex_utils_cargo_bin::cargo_bin("codex-app-server")?,
        &app_server,
    )?;
    copy_with_permissions(zsh_path, &package_zsh_path)?;
    Ok(app_server)
}

fn packaged_zsh_path(codex_home: &Path) -> PathBuf {
    codex_home
        .join("test-package")
        .join("codex-resources")
        .join("zsh")
        .join("bin")
        .join("zsh")
}

fn command_packaged_zsh_path(codex_home: &Path) -> PathBuf {
    let path = packaged_zsh_path(codex_home);
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn copy_with_permissions(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::copy(source, destination)?;
    std::fs::set_permissions(destination, std::fs::metadata(source)?.permissions())
}

fn create_config_toml(
    codex_home: &Path,
    server_uri: &str,
    approval_policy: &str,
    feature_flags: &BTreeMap<Feature, bool>,
) -> std::io::Result<()> {
    MockResponsesConfig::new(server_uri)
        .with_approval_policy(approval_policy)
        .disable_feature(Feature::RemoteModels)
        .with_features(feature_flags)
        .write(codex_home)
}

fn find_test_zsh_path() -> Result<Option<std::path::PathBuf>> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let dotslash_zsh = repo_root.join("codex-rs/app-server/tests/suite/zsh");
    if !dotslash_zsh.is_file() {
        eprintln!(
            "skipping zsh fork test: shared zsh DotSlash file not found at {}",
            dotslash_zsh.display()
        );
        return Ok(None);
    }
    match core_test_support::fetch_dotslash_file(&dotslash_zsh, /*dotslash_cache*/ None) {
        Ok(path) => return Ok(Some(path)),
        Err(error) => {
            eprintln!("failed to fetch vendored zsh via dotslash: {error:#}");
        }
    }

    Ok(None)
}

fn supports_exec_wrapper_intercept(zsh_path: &Path) -> bool {
    let status = std::process::Command::new(zsh_path)
        .arg("-fc")
        .arg("/usr/bin/true")
        .env("EXEC_WRAPPER", "/usr/bin/false")
        .status();
    match status {
        Ok(status) => !status.success(),
        Err(_) => false,
    }
}
