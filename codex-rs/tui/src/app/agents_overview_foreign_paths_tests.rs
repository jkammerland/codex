use super::*;

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
