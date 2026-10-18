use super::*;
use pretty_assertions::assert_eq;

fn styled_task_fixture() -> (AgentsOverviewRow, codex_team::TeamSnapshot) {
    let snapshot: codex_team::TeamSnapshot = serde_json::from_value(serde_json::json!({
        "control": {"coordinatorId":"coordinator","revision":8,"mode":"stopped"},
        "servers": [{"machineId":"machine","machineLabel":"Machine","serverId":"primary","serverLabel":"Primary","installationId":null,"connected":true,"appliedRevision":8,"error":null}],
        "agents": [{
            "enrollment":{"address":{"machineId":"machine","serverId":"primary","threadId":"00000000-0000-7000-8000-000000000001"},"alias":"Reviewer","manuallyHeld":false,"continuationTurnId":null},
            "status":"idle","currentModel":null,"currentEffort":null,"nextModel":"gpt-6.1-sol","nextEffort":"medium","project":null,
            "discovery": {
                "availability":{"synchronization":"current","activity":"idle","readiness":"held","holds":["stopped","continuation"],"reasons":["Team is stopped","An interrupted turn has priority over new work"]},
                "environments":[{"id":"local","current":false,"next":true,"os":"linux","shell":"bash","declaredCapabilities":["compiler"]}]
            },
            "runtime":null
        }],
        "messages":[]
    })).unwrap();
    let mut rows = Vec::new();
    append_enrolled_rows(&snapshot, &mut rows);
    let mut row = rows.remove(0);
    row.metadata_known = true;
    row.thread.reasoning_effort = Some(codex_protocol::openai_models::ReasoningEffort::Medium);
    (row, snapshot)
}

fn assert_span_style(lines: &[Line<'_>], text: &str, style: Style) {
    let span = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content == text)
        .unwrap_or_else(|| panic!("missing span {text:?}"));
    assert_eq!(span.style, style, "style for {text:?}");
}

#[test]
fn held_task_details_have_compact_groups_and_semantic_styles() {
    let (row, snapshot) = styled_task_fixture();
    let details = task_lines(&row, Some(&snapshot));
    assert_eq!(
        details
            .status
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![
            "",
            "Team · Reviewer",
            "Readiness Held · Activity  Idle",
            "Sync      Current",
            "  ↳ Team is stopped",
            "  ↳ An interrupted turn has priority over new work",
            "Server    primary · Control  applied 8",
        ]
    );
    assert_eq!(
        details
            .environment
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![
            "",
            "Environment",
            "local · next",
            "OS  linux · Shell  bash (cached)",
            "Declared  compiler",
        ]
    );
    assert_span_style(&details.status, "Team", Style::default().bold());
    assert_span_style(&details.status, "Readiness ", secondary_text_style());
    assert_span_style(&details.status, "Held", status_style(StatusTone::Attention));
    assert_span_style(
        &details.status,
        "Current",
        status_style(StatusTone::Success),
    );
    assert_span_style(&details.environment, " (cached)", secondary_text_style());
}

#[test]
fn task_details_retain_distinct_current_and_next_model_choices() {
    let (mut row, mut snapshot) = styled_task_fixture();
    let agent = &mut snapshot.agents[0];
    agent.current_model = Some("gpt-6.1-sol".into());
    agent.current_effort = Some("medium".into());
    assert!(execution_lines(&row, agent).is_empty());

    agent.next_effort = Some("high".into());
    assert_eq!(
        execution_lines(&row, agent)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["Next      gpt-6.1-sol / high"]
    );
    agent.current_model = Some("gpt-6-astra".into());
    assert_eq!(
        execution_lines(&row, agent)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![
            "Current   gpt-6-astra / medium",
            "Next      gpt-6.1-sol / high"
        ]
    );

    // An enrollment-only row does not establish historical model settings.
    row.metadata_known = false;
    agent.next_effort = Some("medium".into());
    assert!(
        execution_lines(&row, agent)
            .iter()
            .any(|line| line.to_string() == "Next      gpt-6.1-sol / medium")
    );
}

#[test]
fn task_details_preserve_default_choices_when_header_effort_is_unknown() {
    let (mut row, mut snapshot) = styled_task_fixture();
    row.thread.reasoning_effort = None;
    let agent = &mut snapshot.agents[0];
    agent.current_model = row.thread.model.clone();
    agent.current_effort = None;
    agent.next_effort = None;
    assert_eq!(
        execution_lines(&row, agent)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![
            "Current   gpt-6.1-sol / default",
            "Next      gpt-6.1-sol / default",
        ]
    );
}

#[test]
fn task_details_keep_control_errors_and_unconfirmed_jobs_in_status() {
    let (row, mut snapshot) = styled_task_fixture();
    snapshot.servers[0].error = Some("Host disconnected".into());
    snapshot.servers[0].connected = false;
    let agent = &mut snapshot.agents[0];
    agent.discovery.availability.activity = TeamActivity::WaitingForApproval;
    agent.discovery.availability.synchronization = TeamSynchronization::Disconnected;
    agent.enrollment.manually_held = true;
    agent.requested_selection = Some(TeamModelSelection {
        revision: 9,
        model: TeamSelection::Value {
            value: "queued-model".into(),
        },
        effort: TeamSelection::Default,
    });
    agent.runtime = Some(serde_json::from_value(serde_json::json!({
        "threadId":row.thread.id,"rootThreadId":row.thread.id,"parentThreadId":null,
        "alias":"Reviewer","loaded":true,"archived":false,"turnId":null,
        "currentModel":null,"currentEffort":null,"nextModel":null,"nextEffort":null,
        "modelError":"Model unavailable","appliedSelectionRevision":8,
        "inFlight":2,"reconciling":true,"manuallyHeld":true,"continuationTurnId":null,
        "jobs":[{"id":"job","threadId":row.thread.id,"server":"worker","tool":"launch","handle":null,"status":"unconfirmed","detail":"Cancellation has not been confirmed."}]
    })).unwrap());

    let details = task_lines(&row, Some(&snapshot));
    let lines = details.status;
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "Hold      Manually held",
        "Queued: queued-model / model default · revision 9",
        "Admitted  2 family operation(s)",
        "Settling  Previously admitted work",
        "launch: Unconfirmed · Cancellation has not been confirmed.",
    ] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
    assert!(text.contains("Host disconnected"));
    assert!(!text.contains("Environment"));
    assert!(
        details
            .environment
            .iter()
            .any(|line| line.to_string() == "Environment")
    );
    assert_span_style(
        &lines,
        "Host disconnected",
        status_style(StatusTone::Failure),
    );
    assert_span_style(
        &lines,
        "Model unavailable",
        status_style(StatusTone::Failure),
    );
    assert_span_style(
        &lines,
        "Waiting for approval",
        status_style(StatusTone::Attention),
    );
    assert_span_style(&lines, "Unconfirmed", status_style(StatusTone::Attention));
    assert_span_style(&lines, "Queued: ", status_style(StatusTone::Attention));
}

#[test]
fn task_details_preserve_unknowns_and_plain_tasks_without_hosts() {
    let (mut row, mut snapshot) = styled_task_fixture();
    snapshot.agents[0].discovery = TeamAgentDiscovery::default();
    let details = task_lines(&row, Some(&snapshot));
    assert!(
        details
            .status
            .iter()
            .any(|line| line.to_string() == "Readiness Unknown · Activity  Unknown")
    );
    assert!(
        details
            .status
            .iter()
            .any(|line| line.to_string() == "Sync      Unconfirmed")
    );
    assert!(
        details
            .environment
            .iter()
            .any(|line| line.to_string() == "Unknown")
    );

    row.host = None;
    let details = task_lines(&row, Some(&snapshot));
    assert!(details.status.is_empty());
    assert!(details.environment.is_empty());
}

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
    Team
    Readiness Unknown · Activity  Unknown
    Sync      Unconfirmed

    Environment
    Unknown
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
