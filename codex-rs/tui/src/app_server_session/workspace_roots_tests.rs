//! Verify that remote lifecycle requests use workspace roots owned by the server.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::ConfigOverrides;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ServerNotification;
use codex_utils_path_uri::LegacyAppPathString;
use core_test_support::responses;
use pretty_assertions::assert_eq;

#[test]
fn remote_workspace_roots_survive_start_turn_resume_and_fork() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

    std::thread::Builder::new()
        .name("remote-workspace-roots".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let model_server = responses::start_mock_server().await;
                let response = responses::mount_sse_once(
                    &model_server,
                    responses::sse(vec![
                        responses::ev_response_created("response"),
                        responses::ev_completed("response"),
                    ]),
                )
                .await;
                let server_home = tempfile::tempdir()?;
                let client_home = tempfile::tempdir()?;
                let workspace = tempfile::tempdir()?;
                let remote_cwd =
                    AbsolutePathBuf::from_absolute_path(workspace.path().canonicalize()?)?;
                let extra_root = remote_cwd.join("shared");
                std::fs::create_dir(extra_root.as_path())?;
                let base_url = model_server.uri();
                let extra_root_toml = serde_json::to_string(&extra_root)?;
                std::fs::write(
                    server_home.path().join("config.toml"),
                    format!(
                        r#"
model = "gpt-5.2"
model_provider = "workspace-test"
sandbox_mode = "workspace-write"
[sandbox_workspace_write]
writable_roots = [{extra_root_toml}]
[model_providers.workspace-test]
name = "OpenAI"
base_url = "{base_url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
                    ),
                )?;
                let server_config = ConfigBuilder::default()
                    .codex_home(server_home.path().to_path_buf())
                    .harness_overrides(ConfigOverrides {
                        cwd: Some(remote_cwd.to_path_buf()),
                        ..Default::default()
                    })
                    .build()
                    .await?;
                let client_config = ConfigBuilder::default()
                    .codex_home(client_home.path().to_path_buf())
                    .harness_overrides(ConfigOverrides {
                        cwd: Some(client_home.path().to_path_buf()),
                        ..Default::default()
                    })
                    .build()
                    .await?;
                let local_settings = LocalSettings::from(&client_config);
                let expected_roots: Vec<LegacyAppPathString> =
                    vec![remote_cwd.clone().into(), extra_root.clone().into()];
                let mut app_server =
                    crate::start_embedded_app_server_for_picker(&server_config).await?;
                // Exercise remote request semantics with a real server and separate client config.
                app_server.thread_params_mode = ThreadParamsMode::Remote;
                app_server.remote_cwd_override = Some(remote_cwd.to_path_buf());
                let started = app_server.start_thread(&client_config).await?;
                let thread_id = started.session.thread_id;
                assert_eq!(started.session.cwd, remote_cwd.clone().into());
                assert_eq!(started.session.runtime_workspace_roots, expected_roots);

                let (mut chat, _sender, _events, _commands) =
                    crate::chatwidget::tests::helpers::make_chatwidget_manual_with_sender().await;
                chat.remote_connection =
                    Some(crate::status::remote_connection::RemoteConnectionStatus {
                        address: "wss://worker.invalid".into(),
                        version: "test".into(),
                        is_local_daemon: false,
                        is_managed_daemon: false,
                    });
                let client_paths = (
                    chat.config_ref().cwd.clone(),
                    chat.config_ref().workspace_roots.clone(),
                );
                chat.handle_thread_session(started.session);
                let config = chat.config_ref();
                assert_eq!(
                    (config.cwd.clone(), config.workspace_roots.clone()),
                    client_paths
                );
                app_server
                    .turn_start(
                        thread_id,
                        "user-message".to_string(),
                        vec![UserInput::Text {
                            text: "hello".to_string(),
                            text_elements: Vec::new(),
                        }],
                        config.cwd.to_path_buf(),
                        /*approval_policy*/ None,
                        /*approvals_reviewer*/ None,
                        TurnPermissionsOverride::Preserve,
                        config.permissions.user_visible_workspace_roots(),
                        "gpt-5.2".to_string(),
                        /*effort*/ None,
                        /*summary*/ None,
                        /*service_tier*/ None,
                        /*collaboration_mode*/ None,
                        /*output_schema*/ None,
                        /*cyber_access_program*/ None,
                    )
                    .await?;
                tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 30), async {
                    while let Some(event) = app_server.next_event().await {
                        if let AppServerEvent::ServerNotification(notification) = event.event
                            && let ServerNotification::TurnCompleted(completed) = *notification
                        {
                            assert_eq!(
                                completed.turn.status,
                                codex_app_server_protocol::TurnStatus::Completed
                            );
                            return;
                        }
                    }
                    panic!("app-server disconnected before completing the turn");
                })
                .await?;
                response.single_request();

                let resumed = app_server
                    .resume_thread(
                        &local_settings,
                        client_config.clone(),
                        thread_id,
                        ResumeModelSettings::RestoreFromThread,
                    )
                    .await?;
                assert_eq!(resumed.session.runtime_workspace_roots, expected_roots);
                let forked = app_server
                    .fork_thread(&local_settings, client_config.clone(), thread_id)
                    .await?;
                assert_eq!(forked.session.runtime_workspace_roots, expected_roots);

                // Existing tasks may retain roots that are no longer in the server defaults.
                let config_path = server_home.path().join("config.toml");
                let updated_config = std::fs::read_to_string(&config_path)?.replace(
                    &format!("writable_roots = [{extra_root_toml}]"),
                    "writable_roots = []",
                );
                std::fs::write(config_path, updated_config)?;
                let session_config = chat.config_ref().clone();
                let forked = app_server
                    .fork_thread_at(
                        &local_settings,
                        session_config.clone(),
                        thread_id,
                        /*last_turn_id*/ None,
                        /*before_turn_id*/ None,
                        ForkGoalContinuation::StartIfIdle,
                        /*selected_profile*/ None,
                    )
                    .await?;
                assert_eq!(forked.session.runtime_workspace_roots, expected_roots);
                let side = app_server
                    .fork_side_thread(
                        &local_settings,
                        session_config,
                        thread_id,
                        /*selected_profile*/ None,
                    )
                    .await?;
                assert_eq!(side.session.runtime_workspace_roots, expected_roots);

                app_server.remote_cwd_override = None;
                let started = app_server.start_thread(&client_config).await?;
                let default_cwd = AbsolutePathBuf::current_dir()?;
                assert_eq!(started.session.cwd, default_cwd.clone().into());
                assert_eq!(
                    started.session.runtime_workspace_roots,
                    vec![default_cwd.into()]
                );
                app_server.shutdown().await?;
                Ok(())
            })
        })?
        .join()
        .expect("remote workspace roots test thread")
}

#[tokio::test]
async fn embedded_lifecycle_requests_preserve_explicit_workspace_roots() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            additional_writable_roots: vec![home.path().to_path_buf()],
            ..Default::default()
        })
        .build()
        .await?;
    let expected = Some(config.workspace_roots.clone());
    let thread_id = ThreadId::new();
    let start = thread_start_params_from_config(
        &config,
        ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
        /*session_start_source*/ None,
    );
    let resume = thread_resume_params_from_config(
        config.clone(),
        thread_id,
        ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
        ResumeModelSettings::RestoreFromThread,
        crate::resume_permissions::ResumePermissions::CURRENT_CONFIG,
    );
    let fork = thread_fork_params_from_config(
        config,
        thread_id,
        ThreadParamsMode::Embedded,
        /*remote_cwd_override*/ None,
    );
    assert_eq!(
        [
            start.runtime_workspace_roots,
            resume.runtime_workspace_roots,
            fork.runtime_workspace_roots
        ],
        [expected.clone(), expected.clone(), expected]
    );
    Ok(())
}

#[tokio::test]
async fn windows_executor_paths_survive_lifecycle_and_normal_turn_omits_client_paths() -> Result<()>
{
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    let home = tempfile::tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let thread_id = ThreadId::new();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let thread = json!({
            "id": thread_id.to_string(), "sessionId": thread_id.to_string(),
            "preview": "", "ephemeral": false, "modelProvider": "worker-provider",
            "createdAt": 1, "updatedAt": 1, "status": {"type": "idle"},
            "cwd": r"C:\work\task", "cliVersion": "test", "source": "appServer", "turns": []
        });
        let mut turn_params = None;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent": "windows-path-test/1.0.0"}),
                "thread/start" => json!({
                    "thread": thread, "model": "worker-model", "modelProvider": "worker-provider",
                    "cwd": r"C:\work\task", "runtimeWorkspaceRoots": [r"C:\work\task", r"D:\shared"],
                    "approvalPolicy": "never", "approvalsReviewer": "user",
                    "sandbox": {"type": "workspaceWrite", "writableRoots": [r"D:\shared"],
                        "networkAccess": false, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true}
                }),
                "turn/start" => {
                    turn_params = request.params;
                    json!({"turn": {"id": "turn-1", "items": [], "status": "inProgress"}})
                }
                method => panic!("unexpected request: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id": request.id, "result": result})
                        .to_string()
                        .into(),
                ))
                .await?;
        }
        Ok::<_, color_eyre::Report>(turn_params.expect("normal turn request"))
    });
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Remote,
    );
    let started = session.start_thread(&config).await?;
    assert_eq!(
        (started.session.cwd, started.session.runtime_workspace_roots),
        (
            LegacyAppPathString::from_string(r"C:\work\task"),
            vec![
                LegacyAppPathString::from_string(r"C:\work\task"),
                LegacyAppPathString::from_string(r"D:\shared"),
            ],
        )
    );
    session
        .turn_start(
            thread_id,
            "user-message".into(),
            vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            config.cwd.to_path_buf(),
            None,
            None,
            TurnPermissionsOverride::Preserve,
            &config.workspace_roots,
            "worker-model".into(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    session.shutdown().await?;
    let params = server.await??;
    assert_eq!(
        (params.get("cwd"), params.get("runtimeWorkspaceRoots")),
        (
            Some(&serde_json::Value::Null),
            Some(&serde_json::Value::Null)
        ),
        "the actual remote RPC must preserve executor settings rather than send local config paths"
    );
    Ok(())
}
