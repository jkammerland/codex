//! Executor paths must survive app routing while frontend config stays local.

use super::session_lifecycle_requests::recorded_params;
use super::session_lifecycle_requests::start_recording_remote_app_server;
use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn mark_remote(app: &mut App) {
    app.app_server_target = AppServerTarget::Remote {
        endpoint: crate::RemoteAppServerEndpoint::WebSocket {
            websocket_url: "ws://127.0.0.1:1".into(),
            auth_token: None,
        },
    };
    app.chat_widget.remote_connection =
        crate::status::remote_connection::remote_connection_status_value(
            &app.app_server_target,
            /*server_version*/ None,
        );
}

#[tokio::test]
async fn remote_skills_refresh_dispatches_and_reports_executor_errors() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let workspace = tempdir()?;

    // Normalize Windows namespace aliases before sharing this native path with the session.
    let executor_cwd =
        AbsolutePathBuf::from_absolute_path(workspace.path().canonicalize()?)?.into_path_buf();
    let broken_skill = executor_cwd.join(".agents/skills/broken/SKILL.md");
    std::fs::create_dir_all(broken_skill.parent().expect("skill directory"))?;
    std::fs::write(
        &broken_skill,
        "---\nname: broken\n---\nMissing description.\n",
    )?;
    let local_cwd = app.chat_widget.config_ref().cwd.clone();
    assert_ne!(executor_cwd, local_cwd.as_path());
    let mut server_config = app.config.clone();
    server_config.cwd = executor_cwd.clone().abs();
    let (mut server, requests, proxy) = start_recording_remote_app_server(&server_config).await?;
    mark_remote(&mut app);
    let thread_id = ThreadId::new();
    app.active_thread_id = Some(thread_id);
    app.chat_widget
        .handle_thread_session(test_thread_session(thread_id, executor_cwd.clone()));
    while events.try_recv().is_ok() {}

    app.submit_thread_op(
        &mut server,
        thread_id,
        AppCommand::list_skills(vec![executor_cwd.clone()], /*force_reload*/ true),
    )
    .await?;

    assert_eq!(
        recorded_params(&requests, "skills/list"),
        vec![serde_json::json!({"cwds": [executor_cwd], "forceReload": true})]
    );
    let warnings = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => {
                // Startup warnings intentionally hide their inline display, but retain diagnostics.
                Some(lines_to_single_string(
                    &cell.transcript_lines(/*width*/ 160),
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        warnings.contains("broken/SKILL.md") || warnings.contains(r"broken\SKILL.md"),
        "{warnings}"
    );
    assert_eq!(app.chat_widget.config_ref().cwd, local_cwd);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn foreign_diff_result_opens_overlay_and_rejects_stale_local_result() -> Result<()> {
    let mut app = make_test_app().await;
    let (mut server, _requests, proxy) = start_recording_remote_app_server(&app.config).await?;
    mark_remote(&mut app);
    let mut session = test_thread_session(ThreadId::new(), app.config.cwd.to_path_buf());
    session.cwd = LegacyAppPathString::from_string(r"C:\fleet\workspace");
    app.chat_widget.handle_thread_session(session);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let diff = "+executor-owned change";

    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::DiffResult(PathBuf::from(r"C:\fleet\workspace"), diff.into()),
    ))
    .await?;
    let Some(Overlay::Static(overlay)) = app.overlay.as_mut() else {
        panic!("the matching executor diff must complete and open its pager");
    };
    let rendered = render_diff_overlay(overlay)?;
    assert!(rendered.contains(diff), "{rendered}");

    let local_cwd = app.config.cwd.to_path_buf();
    Box::pin(app.handle_event(
        &mut tui,
        &mut server,
        AppEvent::DiffResult(local_cwd, "stale local diff".into()),
    ))
    .await?;
    let Some(Overlay::Static(overlay)) = app.overlay.as_mut() else {
        panic!("a stale result must retain the matching diff");
    };
    assert_eq!(render_diff_overlay(overlay)?, rendered);
    server.shutdown().await?;
    proxy.await??;
    Ok(())
}

fn render_diff_overlay(overlay: &mut crate::pager_overlay::StaticOverlay) -> Result<String> {
    let mut terminal = Terminal::new(TestBackend::new(/*width*/ 100, /*height*/ 20))?;
    terminal.draw(|frame| overlay.render(frame.area(), frame.buffer_mut()))?;
    Ok(terminal
        .backend()
        .buffer()
        .content
        .chunks(100)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[tokio::test]
async fn session_and_clear_headers_show_the_executor_directory() {
    let mut app = make_test_app().await;
    mark_remote(&mut app);
    let mut session = test_thread_session(ThreadId::new(), app.config.cwd.to_path_buf());
    session.cwd = LegacyAppPathString::from_string(r"C:\fleet\executor-workspace");
    app.chat_widget.handle_thread_session(session.clone());
    let info = history_cell::new_session_info(
        &app.config,
        &app.local_settings,
        "gpt-test",
        "gpt-test",
        &session,
        /*is_first_event*/ false,
        /*tooltip_override*/ None,
        /*auth_plan*/ None,
    );
    let info = lines_to_single_string(&info.display_lines(/*width*/ 160));
    let cleared = lines_to_single_string(&app.clear_ui_header_lines(/*width*/ 160));

    for header in [info, cleared] {
        assert!(header.contains(r"C:\fleet\executor-workspace"), "{header}");
    }
}
