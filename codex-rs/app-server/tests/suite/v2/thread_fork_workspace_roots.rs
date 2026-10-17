use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use test_case::test_case;

fn normalized_workspace(workspace: &TempDir) -> Result<AbsolutePathBuf> {
    Ok(AbsolutePathBuf::from_absolute_path(
        workspace.path().canonicalize()?,
    )?)
}

#[test_case(ThreadHistoryMode::Legacy, false; "loaded_legacy")]
#[test_case(ThreadHistoryMode::Legacy, true; "cold_legacy")]
#[test_case(ThreadHistoryMode::Paginated, false; "loaded_paginated")]
#[test_case(ThreadHistoryMode::Paginated, true; "cold_paginated")]
#[tokio::test]
async fn thread_fork_restores_runtime_workspace_roots(
    history_mode: ThreadHistoryMode,
    restart_before_fork: bool,
) -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let cwd = normalized_workspace(&workspace)?;
    let initial_extra = TempDir::new()?;
    let initial_root = normalized_workspace(&initial_extra)?;
    let updated_extra = TempDir::new()?;
    let updated_root = normalized_workspace(&updated_extra)?;
    let retargeted_workspace = TempDir::new()?;
    let retargeted_cwd = normalized_workspace(&retargeted_workspace)?;
    MockResponsesConfig::new(&server.uri())
        .with_extra_config(&format!(
            "[sandbox_workspace_write]\nwritable_roots = [{}]",
            serde_json::to_string(
                initial_root
                    .as_path()
                    .to_str()
                    .expect("UTF-8 fixture workspace")
            )?,
        ))
        .write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            history_mode: Some(history_mode),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            runtime_workspace_roots: Some(vec![cwd.clone(), initial_root.clone()]),
            ..Default::default()
        })
        .await?;
    let started: ThreadStartResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(start_id)).await??;
    assert_eq!(
        started.runtime_workspace_roots,
        vec![
            LegacyAppPathString::from(cwd.clone()),
            initial_root.clone().into()
        ],
    );

    // Materialize the startup selection, then record a different current selection after the cutoff.
    let first_turn = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "persist startup workspace selection".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "retain the updated workspace selection".to_string(),
                text_elements: Vec::new(),
            }],
            runtime_workspace_roots: Some(vec![
                cwd.clone(),
                updated_root.clone(),
                updated_root.clone(),
            ]),
            ..Default::default()
        }),
    )
    .await??;

    // Server defaults may change after a task selects its roots; a fork still inherits that task.
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    if restart_before_fork {
        timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;
        mcp = TestAppServer::builder()
            .with_codex_home(codex_home.path())
            .without_auto_env()
            .build_initialized()
            .await?;
    }
    for (cutoff, cwd_override, roots_override, expected_cwd, expected_roots) in [
        (
            None,
            None,
            None,
            cwd.clone(),
            vec![cwd.clone(), updated_root.clone()],
        ),
        (
            Some(first_turn.turn.id.clone()),
            None,
            None,
            cwd.clone(),
            vec![cwd.clone(), updated_root.clone()],
        ),
        (
            Some(first_turn.turn.id.clone()),
            Some(retargeted_cwd.clone()),
            None,
            retargeted_cwd.clone(),
            vec![retargeted_cwd.clone(), updated_root.clone()],
        ),
        (None, None, Some(Vec::new()), cwd.clone(), Vec::new()),
        (
            None,
            Some(retargeted_cwd.clone()),
            Some(vec![initial_root.clone()]),
            retargeted_cwd.clone(),
            vec![initial_root.clone()],
        ),
        // The preceding fork overrides must not mutate the source's selection.
        (
            None,
            None,
            None,
            cwd.clone(),
            vec![cwd.clone(), updated_root.clone()],
        ),
    ] {
        // Cutoff cases also force permission restoration off, so roots alone require current history.
        let explicit_policy = cutoff.is_some();
        let fork_id = mcp
            .send_thread_fork_request(ThreadForkParams {
                thread_id: started.thread.id.clone(),
                last_turn_id: cutoff,
                cwd: cwd_override.map(|cwd| cwd.to_string_lossy().into_owned()),
                runtime_workspace_roots: roots_override,
                approval_policy: explicit_policy.then_some(AskForApproval::Never),
                approvals_reviewer: explicit_policy.then_some(ApprovalsReviewer::User),
                sandbox: explicit_policy.then_some(SandboxMode::ReadOnly),
                ..Default::default()
            })
            .await?;
        let forked: ThreadForkResponse =
            timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(fork_id)).await??;
        assert_eq!(forked.cwd, LegacyAppPathString::from(expected_cwd));
        if explicit_policy {
            assert_eq!(forked.approval_policy, AskForApproval::Never);
            assert_eq!(forked.approvals_reviewer, ApprovalsReviewer::User);
            assert!(matches!(forked.sandbox, SandboxPolicy::ReadOnly { .. }));
        }
        assert_eq!(
            forked.runtime_workspace_roots,
            expected_roots
                .into_iter()
                .map(LegacyAppPathString::from)
                .collect::<Vec<_>>(),
        );
    }

    timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;
    Ok(())
}

#[tokio::test]
async fn thread_fork_restores_startup_runtime_workspace_roots() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    let workspace = TempDir::new()?;
    let cwd = normalized_workspace(&workspace)?;
    let extra_workspace = TempDir::new()?;
    let extra_root = normalized_workspace(&extra_workspace)?;
    let source_id = create_fake_rollout(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved startup workspace roots",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let source_path = rollout_path(codex_home.path(), "2025-01-05T12-00-00", &source_id);
    let contents = std::fs::read_to_string(&source_path)?;
    let (meta, history) = contents.split_once('\n').expect("rollout metadata line");
    let mut meta: Value = serde_json::from_str(meta)?;
    meta["payload"]["cwd"] = json!(cwd);
    meta["payload"]["runtime_workspace_roots"] = json!([cwd, extra_root, extra_root]);
    std::fs::write(&source_path, format!("{meta}\n{history}"))?;
    let original_contents = std::fs::read_to_string(&source_path)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build_initialized()
        .await?;
    let fork_id = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: source_id,
            ..Default::default()
        })
        .await?;
    let forked: ThreadForkResponse =
        timeout(DEFAULT_READ_TIMEOUT, mcp.read_response(fork_id)).await??;
    assert_eq!(forked.cwd, LegacyAppPathString::from(cwd.clone()));
    assert_eq!(
        forked.runtime_workspace_roots,
        vec![LegacyAppPathString::from(cwd), extra_root.into()],
    );
    assert_eq!(std::fs::read_to_string(source_path)?, original_contents);
    timeout(DEFAULT_READ_TIMEOUT, mcp.shutdown_gracefully()).await??;
    Ok(())
}
