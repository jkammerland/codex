use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_yolo_runs_forced_deletion_without_approval() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let target = test.cwd_path().join("diagnostic");
    std::fs::create_dir(&target)?;
    std::fs::write(target.join("task-owned.txt"), "temporary test fixture")?;

    // Exercise the native dangerous-command classifier through the real CLI.
    let command = if cfg!(windows) {
        "Remove-Item -LiteralPath diagnostic -Recurse -Force"
    } else {
        "rm -rf diagnostic"
    };
    let shell = if cfg!(windows) {
        let shell = codex_core::shell::default_user_shell();
        assert_eq!(shell.name(), "powershell");
        shell.derive_exec_args(command, /*use_login_shell*/ false)[0].clone()
    } else {
        "/bin/sh".to_string()
    };
    let server = responses::start_mock_server().await;
    let call_id = "yolo-forced-deletion";
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("yolo-command"),
                responses::ev_exec_command_call_with_args(
                    call_id,
                    &serde_json::json!({
                        "cmd": command,
                        "shell": shell,
                        "login": false,
                        "yield_time_ms": 10_000,
                    }),
                ),
                responses::ev_completed("yolo-command"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("yolo-done", "done"),
                responses::ev_completed("yolo-result"),
            ]),
        ],
    )
    .await;

    let output = test
        .cmd_with_server(&server)
        .args([
            "--yolo",
            "--skip-git-repo-check",
            "--ephemeral",
            "delete the diagnostic fixture",
        ])
        .output()?;
    assert!(output.status.success(), "exec run failed: {output:?}");

    // The real CLI must resolve --yolo before policy evaluates the model command.
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("approval: never"), "{stderr}");
    assert!(stderr.contains("sandbox: danger-full-access"), "{stderr}");
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let tool_output = requests[1]
        .function_call_output_text(call_id)
        .expect("forced deletion should produce a tool response");
    assert!(
        tool_output.contains("Process exited with code 0"),
        "{tool_output}"
    );
    assert!(!target.exists(), "task-owned directory was not deleted");

    Ok(())
}
