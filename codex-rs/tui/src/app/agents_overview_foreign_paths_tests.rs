use super::*;
#[cfg(unix)]
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn directory_grouping_does_not_resolve_remote_paths_against_local_aliases() {
    let mut app = make_test_app().await;
    let root = tempfile::tempdir().unwrap();
    let alias = root.path().join("a-alias");
    let middle = root.path().join("m-directory");
    let target = root.path().join("z-target");
    std::fs::create_dir(&middle).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    app.agents_overview.view_state.lock().unwrap().grouping = AgentsOverviewGrouping::Directory;

    for remote in [false, true] {
        app.agents_hosts.manifest = remote.then(|| root.path().join("team.toml"));
        let threads = [(&alias, "Alias task"), (&middle, "Middle task")]
            .into_iter()
            .map(|(cwd, title)| {
                let mut thread = overview_thread(
                    ThreadId::new(),
                    /*parent_thread_id*/ None,
                    title,
                    ThreadStatus::Idle,
                );
                thread.cwd = codex_utils_path_uri::LegacyAppPathString::from_path(cwd);
                thread
            })
            .collect();
        let view = app.agents_overview_view(threads, /*selected_thread_id*/ None);
        app.chat_widget.show_bottom_pane_view(Box::new(view));
        let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 80);
        let alias_position = rendered.find("Alias task").expect("alias task is visible");
        let middle_position = rendered
            .find("Middle task")
            .expect("middle task is visible");

        // Local aliases sort by their resolved directory; team paths retain executor spelling.
        assert_eq!(alias_position < middle_position, remote, "{rendered}");
    }
}

#[tokio::test]
async fn windows_thread_paths_survive_overview_rendering() {
    let app = make_test_app().await;
    let id = ThreadId::from_u128(42);
    let thread = overview_thread(
        id,
        /*parent_thread_id*/ None,
        "Review [the source](C:\\Pilot\\project\\src\\main.rs#L12)",
        ThreadStatus::Idle,
    );

    // Exercise the wire boundary, rather than constructing a host-native test path.
    let mut wire = serde_json::to_value(thread).unwrap();
    wire["cwd"] = serde_json::json!(r"C:\Pilot\project");
    wire["environments"] = serde_json::json!([{
        "environmentId": "local",
        "cwd": r"C:\Pilot\project",
        "runtimeWorkspaceRoots": [r"C:\Pilot\project"]
    }]);
    let thread = serde_json::from_value(wire).unwrap();
    let view = app.agents_overview_view(vec![thread], Some(id));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal
        .draw(|frame| view.render(frame.area(), frame.buffer_mut()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .chunks(100)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains(r"C:\Pilot\project"), "{rendered}");
    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!("agents_overview_windows_paths", rendered);
    });
}
