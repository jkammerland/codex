use super::*;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cached_windows_settings_retarget_only_the_implicit_executor_root() {
    let mut app = crate::app::test_support::make_test_app().await;
    let thread_id = ThreadId::new();
    let thread: codex_app_server_protocol::Thread = serde_json::from_value(serde_json::json!({
        "id": thread_id.to_string(), "sessionId": thread_id.to_string(),
        "preview": "", "ephemeral": false, "modelProvider": "worker-provider",
        "createdAt": 1, "updatedAt": 1, "status": {"type": "idle"},
        "cwd": r"C:\work\before", "cliVersion": "test", "source": "appServer", "turns": [],
        "environments": [{"environmentId": "local", "cwd": r"C:\work\before",
            "runtimeWorkspaceRoots": [r"C:\work\before", r"D:\shared"]}]
    }))
    .expect("Windows thread should deserialize on every client platform");
    let mut session = app.session_state_for_thread_read(thread_id, &thread).await;
    assert_eq!(
        session.runtime_workspace_roots,
        vec![
            LegacyAppPathString::from_string(r"C:\work\before"),
            LegacyAppPathString::from_string(r"D:\shared"),
        ]
    );
    let mut unselected = thread.clone();
    unselected.environments = Some(Vec::new());
    let unselected_session = app
        .session_state_for_thread_read(thread_id, &unselected)
        .await;
    assert_eq!(unselected_session.runtime_workspace_roots, Vec::new());
    let settings: ThreadSettings = serde_json::from_value(serde_json::json!({
        "cwd": r"C:\work\after", "approvalPolicy": "never", "approvalsReviewer": "user",
        "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [r"D:\shared"],
            "networkAccess": false, "excludeTmpdirEnvVar": true, "excludeSlashTmp": true},
        "model": "worker-model", "modelProvider": "worker-provider",
        "collaborationMode": {"mode": "default", "settings": {
            "model": "worker-model", "reasoning_effort": null, "developer_instructions": null}}
    }))
    .expect("executor settings");
    apply_thread_settings_to_session(
        &mut session,
        &settings,
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    assert_eq!(
        (session.cwd, session.runtime_workspace_roots),
        (
            LegacyAppPathString::from_string(r"C:\work\after"),
            vec![
                LegacyAppPathString::from_string(r"C:\work\after"),
                LegacyAppPathString::from_string(r"D:\shared"),
            ],
        )
    );

    // The active widget must also keep the native client paths when the same notification arrives.
    app.chat_widget.remote_connection =
        Some(crate::status::remote_connection::RemoteConnectionStatus {
            address: "wss://worker.invalid".into(),
            version: "test".into(),
            is_local_daemon: false,
            is_managed_daemon: false,
        });
    let session = app.session_state_for_thread_read(thread_id, &thread).await;
    app.chat_widget.handle_thread_session(session);
    let native_paths = (
        app.chat_widget.config_ref().cwd.clone(),
        app.chat_widget.config_ref().workspace_roots.clone(),
    );
    app.chat_widget.on_thread_settings_updated(
        codex_app_server_protocol::ThreadSettingsUpdatedNotification {
            thread_id: thread_id.to_string(),
            thread_settings: settings,
        },
    );
    assert_eq!(
        (
            app.chat_widget.executor_cwd(),
            app.chat_widget.executor_workspace_roots(),
        ),
        (
            std::path::Path::new(r"C:\work\after"),
            vec![
                LegacyAppPathString::from_string(r"C:\work\after"),
                LegacyAppPathString::from_string(r"D:\shared"),
            ],
        )
    );
    assert_eq!(
        (
            app.chat_widget.config_ref().cwd.clone(),
            app.chat_widget.config_ref().workspace_roots.clone()
        ),
        native_paths,
    );
}
