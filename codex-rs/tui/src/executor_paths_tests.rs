use super::*;
use crate::legacy_core::config::ConfigBuilder;
use crate::session_state::ThreadSessionState;
use codex_protocol::ThreadId;
use codex_protocol::permissions::FileSystemPath;
use pretty_assertions::assert_eq;

#[test]
fn windows_display_permissions_keep_executor_roots_on_every_client_platform() {
    let cwd = LegacyAppPathString::from_string(r"C:\work\task");
    let extra = LegacyAppPathString::from_string(r"D:\shared");
    let profile = display_permission_profile_from_app_server(
        &SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![extra],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        },
        &cwd,
        ThreadParamsMode::Remote,
    )
    .expect("foreign paths should be interpreted with their executor convention");
    let mut concrete_paths = profile
        .file_system_sandbox_policy()
        .entries
        .into_iter()
        .filter(|entry| entry.access == codex_protocol::permissions::FileSystemAccessMode::Write)
        .filter_map(|entry| match entry.path {
            FileSystemPath::Path { path } => Some(path.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    concrete_paths.sort();
    assert_eq!(
        concrete_paths,
        vec![
            "file:///C:/work/task".to_owned(),
            "file:///D:/shared".to_owned()
        ]
    );
}

#[tokio::test]
async fn native_config_path_update_is_atomic_when_a_root_is_not_native() {
    let home = tempfile::tempdir().expect("temporary home");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    let original = (config.cwd.clone(), config.workspace_roots.clone());
    let cwd = LegacyAppPathString::from_abs_path(
        &AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute home"),
    );
    let result = sync_local_config_paths(
        &mut config,
        &cwd,
        &[LegacyAppPathString::from_string("relative/executor/root")],
    );
    assert!(result.is_err());
    assert_eq!((config.cwd, config.workspace_roots), original);
}

#[tokio::test]
async fn remote_session_paths_do_not_replace_native_client_config() {
    for raw_cwd in [r"C:\work\task", "/remote/work/task"] {
        let (mut chat, _, _, mut operations) =
            crate::chatwidget::tests::helpers::make_chatwidget_manual_with_sender().await;
        let original = (
            chat.config_ref().cwd.clone(),
            chat.config_ref().workspace_roots.clone(),
        );
        chat.remote_connection = Some(crate::status::remote_connection::RemoteConnectionStatus {
            address: "wss://worker.invalid".into(),
            version: "test".into(),
            is_local_daemon: false,
            is_managed_daemon: false,
        });
        let cwd = LegacyAppPathString::from_string(raw_cwd);
        let session = ThreadSessionState {
            daybreak_enabled: false,
            windows_sandbox_host: crate::app::WindowsSandboxHost::Remote,
            thread_id: ThreadId::new(),
            forked_from_id: None,
            fork_parent_title: None,
            thread_name: None,
            model: "test-model".into(),
            model_provider_id: "test-provider".into(),
            service_tier: None,
            approval_policy: codex_app_server_protocol::AskForApproval::Never,
            approvals_reviewer: codex_protocol::config_types::ApprovalsReviewer::User,
            permission_profile: PermissionProfile::read_only(),
            active_permission_profile: None,
            cwd: cwd.clone(),
            runtime_workspace_roots: vec![cwd],
            instruction_source_paths: Vec::new(),
            reasoning_effort: None,
            collaboration_mode: None,
            message_history: None,
            network_proxy: None,
            rollout_path: None,
        };
        chat.handle_thread_session(session);
        assert_eq!(chat.executor_cwd(), std::path::Path::new(raw_cwd));
        assert_eq!(
            chat.executor_workspace_roots(),
            vec![LegacyAppPathString::from_string(raw_cwd)]
        );
        assert_eq!(
            (
                chat.config_ref().cwd.clone(),
                chat.config_ref().workspace_roots.clone(),
            ),
            original,
            "remote paths must stay separate even when they use the client's path convention"
        );

        // Refresh must query the selected executor rather than the native client config cwd.
        while operations.try_recv().is_ok() {}
        chat.handle_manage_skills_closed();
        let command = operations
            .try_recv()
            .expect("skills refresh must be queued");
        let crate::app_command::AppCommand::ListSkills { cwds, force_reload } = command else {
            panic!("expected a skills refresh command");
        };
        assert_eq!(
            (cwds, force_reload),
            (vec![std::path::PathBuf::from(raw_cwd)], true)
        );
    }
}
