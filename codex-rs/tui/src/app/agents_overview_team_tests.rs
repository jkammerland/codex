use super::*;
use pretty_assertions::assert_eq;

#[test]
fn enrolled_unloaded_thread_remains_visible_without_fabricating_history() {
    let agent: codex_team::Agent = serde_json::from_value(serde_json::json!({
        "enrollment":{"address":{"machineId":"machine","serverId":"server","threadId":"00000000-0000-7000-8000-000000000001"},"alias":"Board reviewer","manuallyHeld":false,"continuationTurnId":null},
        "runtime":null,"status":"unloaded","currentModel":null,"currentEffort":null,"nextModel":null,"nextEffort":null,"project":null
    })).unwrap();
    let mut snapshot = codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 0,
            mode: TeamExecutionMode::Stopped,
        },
        servers: vec![],
        agents: vec![agent],
        messages: vec![],
        board: Default::default(),
        rejected_outbox: vec![],
    };
    let mut rows = Vec::new();
    append_enrolled_rows(&snapshot, &mut rows);
    append_enrolled_rows(&snapshot, &mut rows);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].thread.name.as_deref(), Some("Board reviewer"));
    assert_eq!(rows[0].thread.status, ThreadStatus::NotLoaded);
    assert_eq!(rows[0].thread.updated_at, 0);
    assert_eq!(rows[0].thread.model, None);
    assert!(!rows[0].metadata_known);
    snapshot.agents[0].status = "archived".into();
    let mut archived_rows = Vec::new();
    append_enrolled_rows(&snapshot, &mut archived_rows);
    assert!(archived_rows.is_empty());
    insta::assert_snapshot!(discovery_lines(&TeamAgentDiscovery::default()).iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"), @r###"
    Synchronization: Unconfirmed
    Activity: Unknown · Readiness: Unknown
    Environments: unknown
    "###);
}

#[test]
fn enrolled_row_renders_unknown_history_and_discovery_at_wide_and_narrow_widths() {
    use crate::app::agents_overview_view::AgentsOverviewView;
    use crate::render::renderable::Renderable;
    let agent: codex_team::Agent = serde_json::from_value(serde_json::json!({
        "enrollment":{"address":{"machineId":"machine","serverId":"server","threadId":"00000000-0000-7000-8000-000000000001"},"alias":"Board reviewer","manuallyHeld":false,"continuationTurnId":null},
        "runtime":null,"status":"unloaded","currentModel":null,"currentEffort":null,"nextModel":null,"nextEffort":null,"project":null
    })).unwrap();
    let snapshot = codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 0,
            mode: TeamExecutionMode::Stopped,
        },
        servers: vec![],
        agents: vec![agent],
        messages: vec![],
        board: Default::default(),
        rejected_outbox: vec![],
    };
    let mut rows = Vec::new();
    append_enrolled_rows(&snapshot, &mut rows);
    rows[0]
        .details
        .lines
        .extend(discovery_lines(&TeamAgentDiscovery::default()));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut view = AgentsOverviewView::new(
        rows,
        None,
        false,
        false,
        crate::app_event_sender::AppEventSender::new(sender),
        crate::keymap::RuntimeKeymap::defaults(),
        Default::default(),
    );
    use crate::bottom_pane::BottomPaneView;
    for key in ['n', 'w', 'f', 'r', 'p'] {
        view.handle_key_event(crossterm::event::KeyCode::Char(key).into());
        assert!(
            receiver.try_recv().is_err(),
            "metadata-dependent key {key} emitted an operation"
        );
    }
    let mut screens = Vec::new();
    for width in [100, 48] {
        let area = ratatui::layout::Rect::new(0, 0, width, 18);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut buffer);
        screens.push(
            buffer
                .content
                .chunks(usize::from(width))
                .map(|row| {
                    row.iter()
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    insta::assert_snapshot!(screens.join("\n---\n"));

    // Help also computes selection-dependent hints after releasing the rendering guard.
    view.handle_key_event(crossterm::event::KeyCode::Char('?').into());
    let help_area = ratatui::layout::Rect::new(0, 0, 100, 26);
    let mut help_buffer = ratatui::buffer::Buffer::empty(help_area);
    view.render(help_area, &mut help_buffer);
    let help = help_buffer
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(help.contains("Task shortcuts"));
    assert!(help.contains("Resume"));
    assert!(!help.contains("New worktree"));
    assert!(!help.contains("Fork"));
    assert!(!help.contains("Rename"));
    view.handle_key_event(crossterm::event::KeyCode::Esc.into());

    view.handle_key_event(crossterm::event::KeyCode::Enter.into());
    assert!(
        matches!(receiver.try_recv().unwrap(), crate::app_event::AppEvent::AgentsHostAction { event, .. } if matches!(*event, crate::app_event::AppEvent::SelectAgentsOverviewThread { .. }))
    );
}

#[tokio::test]
async fn enrollment_snapshot_does_not_resurrect_removed_foreground_task() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(42);
    let host = app.agents_hosts.foreground.clone();
    let agent: codex_team::Agent = serde_json::from_value(serde_json::json!({
        "enrollment":{"address":{"machineId":host.machine_id,"serverId":host.server_id,"threadId":id.to_string()},"alias":"Removed reviewer","manuallyHeld":false,"continuationTurnId":null},
        "runtime":null,"status":"unloaded","currentModel":null,"currentEffort":null,"nextModel":null,"nextEffort":null,"project":null
    })).unwrap();
    app.agents_hosts.snapshot = Some(codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 0,
            mode: TeamExecutionMode::Stopped,
        },
        servers: vec![],
        agents: vec![agent],
        messages: vec![],
        board: Default::default(),
        rejected_outbox: vec![],
    });
    assert_eq!(
        app.agents_overview_view(Vec::new(), None).thread_ids(),
        vec![id]
    );
    app.agents_overview.removed_threads.insert(id);
    assert!(
        app.agents_overview_view(Vec::new(), None)
            .thread_ids()
            .is_empty()
    );
}
