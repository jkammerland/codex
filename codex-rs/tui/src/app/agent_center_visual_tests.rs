//! Wide host regions and compact layouts keep status and connection text available.

use super::*;
use crate::app::agents_hosts::HostKey;
use crate::app::agents_overview_view::command_center::hosts::HostSummary;
use pretty_assertions::assert_eq;

async fn mixed_status_hosts_view() -> AgentsOverviewView {
    let app = make_test_app().await;
    app.agents_overview.view_state.lock().unwrap().grouping = AgentsOverviewGrouping::Status;
    let hosts = [
        ("a-archive", "primary", "Archive / Private worker", true),
        ("b-local", "normal", "Local / Normal daemon", true),
        ("b-local", "primary", "Local / Private worker", true),
        ("c-laptop", "primary", "Laptop / Private worker", true),
        ("d-builder", "primary", "Builder / Private worker", true),
        ("z-offline", "primary", "Offline / Private worker", false),
    ]
    .map(|(machine, server, label, connected)| HostSummary {
        key: HostKey {
            machine_id: machine.into(),
            server_id: server.into(),
        },
        label: label.into(),
        connected,
    });
    let cases = [
        (0, "Archived review", ThreadStatus::NotLoaded),
        (
            1,
            "Build library",
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
        ),
        (1, "Review results", ThreadStatus::Idle),
        (1, "Prior task", ThreadStatus::NotLoaded),
        (2, "Team backlog", ThreadStatus::NotLoaded),
        (3, "Laptop backlog", ThreadStatus::NotLoaded),
        (4, "Builder backlog", ThreadStatus::NotLoaded),
    ];
    let threads = cases
        .iter()
        .enumerate()
        .map(|(index, (_, title, status))| {
            let mut thread = overview_thread(
                ThreadId::from_u128(index as u128 + 1),
                None,
                title,
                status.clone(),
            );
            thread.updated_at = 4_000_000_000;
            thread
        })
        .collect();
    let mut view = app.agents_overview_view(threads, Some(ThreadId::from_u128(1)));
    for row in &mut view.rows {
        let (host, _, _) = cases
            .iter()
            .find(|(_, title, _)| row.thread.name.as_deref() == Some(*title))
            .unwrap();
        row.host = Some(hosts[*host].key.clone());
        row.machine_label = Some(hosts[*host].label.clone());
    }
    app.agents_overview.view_state.lock().unwrap().hosts = hosts.to_vec();
    view
}

#[tokio::test]
async fn status_groups_preserve_host_boundaries_and_task_hierarchy() {
    let mut frames = Vec::new();
    for (width, height) in [(80, 36), (120, 36), (160, 40)] {
        let view = mixed_status_hosts_view().await;
        let output = screen(&view, width, height);
        let lines = output.lines().collect::<Vec<_>>();
        let first_task = lines
            .iter()
            .position(|line| line.contains("○ Archived review"))
            .expect("the first host's inactive task is visible");
        let dot_byte = lines[first_task].find("○ Archived review").unwrap();
        let host_column = lines[first_task][..dot_byte].chars().count() - 4;

        // Each first subgroup belongs directly beneath its host, including adjacent inactive hosts.
        for (index, (host, group, task)) in [
            ("Archive / Private worker", "Inactive", "○ Archived review"),
            ("Local / Normal daemon", "Working", "● Build library"),
            ("Local / Private worker", "Inactive", "○ Team backlog"),
            ("Laptop / Private worker", "Inactive", "○ Laptop backlog"),
            ("Builder / Private worker", "Inactive", "○ Builder backlog"),
        ]
        .into_iter()
        .enumerate()
        {
            let task_row = lines.iter().position(|line| line.contains(task)).unwrap();
            let heading = lines[task_row - 2]
                .chars()
                .skip(host_column)
                .collect::<String>();
            assert!(heading.starts_with(host), "{output}");
            let subgroup = lines[task_row - 1]
                .chars()
                .skip(host_column)
                .collect::<String>();
            assert!(subgroup.starts_with(&format!("│ {group}")), "{output}");
            let task_line = lines[task_row]
                .chars()
                .skip(host_column)
                .collect::<String>();
            assert!(task_line.starts_with('│'), "{output}");
            assert_eq!(
                task_line
                    .chars()
                    .skip(4)
                    .take(task.chars().count())
                    .collect::<String>(),
                task
            );
            if index > 0 {
                assert!(
                    lines[task_row - 3]
                        .chars()
                        .skip(host_column)
                        .collect::<String>()
                        .starts_with("────────────"),
                    "a separator must precede the next host, not split it from its subgroup:\n{output}"
                );
            }
        }

        // Working, idle and inactive remain separate subgroups inside the normal daemon.
        let task_rows = ["● Build library", "○ Review results", "○ Prior task"]
            .map(|task| lines.iter().position(|line| line.contains(task)).unwrap());
        assert_eq!(task_rows[1] - task_rows[0], 3, "{output}");
        assert_eq!(task_rows[2] - task_rows[1], 3, "{output}");
        for row in [task_rows[0] + 1, task_rows[1] + 1] {
            assert_eq!(lines[row].chars().nth(host_column), Some('│'), "{output}");
        }
        frames.push(format!("{width}×{height}\n{output}"));
    }
    insta::assert_snapshot!(frames.join("\n\n"));
}

#[tokio::test]
async fn short_host_sections_keep_paged_task_selection_visible() {
    let mut frames = Vec::new();
    for width in [80, 120, 160] {
        let mut view = mixed_status_hosts_view().await;
        let before = view.selected_index();
        let _ = screen(&view, width, 12);
        for key in [KeyCode::PageDown, KeyCode::End] {
            view.handle_key_event(key.into());
            assert_ne!(
                view.selected_index(),
                before,
                "navigation must reach another task"
            );
            let output = screen(&view, width, 12);
            let selected = view.rows[view.selected_index().expect("a task is selected")]
                .thread
                .name
                .as_deref()
                .unwrap();
            let selected_row = output.lines().find(|line| {
                line.contains(selected) && (line.contains("› ○") || line.contains("› ●"))
            });
            assert!(
                selected_row.is_some(),
                "selected task {selected} must remain visible:\n{output}"
            );
            frames.push(format!("{width}×12 {key:?}\n{output}"));
        }
        assert_eq!(
            view.rows[view.selected_index().expect("a task is selected")]
                .thread
                .name
                .as_deref(),
            Some("Builder backlog")
        );
    }
    insta::assert_snapshot!(frames.join("\n\n"));
}

#[tokio::test]
async fn team_status_keeps_navigation_hints_visible_at_full_and_short_heights() {
    let mut app = make_test_app().await;
    app.agents_hosts.snapshot = Some(
        serde_json::from_value(serde_json::json!({
            "control": { "coordinatorId": "test", "revision": 3, "mode": "running" },
            "servers": [{
                "machineId": "desktop", "machineLabel": "Desktop",
                "serverId": "normal", "serverLabel": "Normal daemon",
                "connected": true, "appliedRevision": 3
            }],
            "agents": [], "messages": []
        }))
        .unwrap(),
    );
    let id = ThreadId::from_u128(11);
    let view = app.agents_overview_view(
        vec![overview_thread(
            id,
            None,
            "Visible task",
            ThreadStatus::Idle,
        )],
        Some(id),
    );

    for (width, height) in [(160, 40), (120, 12), (80, 12)] {
        let output = screen(&view, width, height);
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), usize::from(height));
        assert!(output.contains("Visible task"), "{output}");
        assert!(
            lines[usize::from(height) - 2].contains("Team Running · 1/1 hosts applied"),
            "{output}"
        );
        let footer = lines.last().unwrap();
        assert!(footer.contains("esc back"), "{output}");
        assert!(footer.contains("⌃d quit"), "{output}");
    }
}

#[tokio::test]
async fn host_regions_adapt_to_wide_narrow_and_short_terminals() {
    let app = make_test_app().await;
    let threads = [
        ("Daemon conversation", ThreadStatus::Idle),
        (
            "Native build",
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
        ),
        ("Review held", ThreadStatus::Idle),
        ("Offline review", ThreadStatus::Idle),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (title, status))| {
        let mut thread =
            overview_thread(ThreadId::from_u128(index as u128 + 1), None, title, status);
        thread.updated_at = 4_000_000_000;
        thread
    })
    .collect();
    let mut view = app.agents_overview_view(threads, Some(ThreadId::from_u128(1)));
    let hosts = [
        ("desktop", "normal", "Desktop / Normal daemon", true),
        ("desktop", "primary", "Desktop / Team worker", true),
        ("laptop", "primary", "Laptop / Team worker", false),
        ("builder", "primary", "Builder / Team worker", false),
    ]
    .map(|(machine, server, label, connected)| HostSummary {
        key: HostKey {
            machine_id: machine.into(),
            server_id: server.into(),
        },
        label: label.into(),
        connected,
    });
    for row in &mut view.rows {
        let host = match row.thread.name.as_deref() {
            Some("Daemon conversation") => &hosts[0],
            Some("Offline review") => &hosts[2],
            _ => &hosts[1],
        };
        row.host = Some(host.key.clone());
        row.machine_label = Some(host.label.clone());
        if row.thread.name.as_deref() == Some("Review held") {
            row.details.availability = Some(serde_json::from_value(serde_json::json!({
                "synchronization": "current", "activity": "idle", "readiness": "held", "holds": ["manual"], "reasons": []
            })).unwrap());
        }
    }
    app.agents_overview.view_state.lock().unwrap().hosts = hosts.to_vec();
    let frames = [(160, 27), (100, 20), (45, 14), (128, 7)].map(|(width, height)| {
        let output = screen(&view, width, height);
        assert!(output.contains("Tasks"), "{output}");
        if width >= 128 && height >= 20 {
            assert!(
                output.contains("Builder / Team worker"),
                "empty offline hosts remain visible: {output}"
            );
            assert!(output.contains("Held"), "{output}");
            assert!(output.contains("Offline"), "{output}");
        }
        format!("{width}×{height}\n{output}")
    });
    insta::assert_snapshot!(frames.join("\n\n"));
}

#[tokio::test]
async fn selected_status_colors_survive_dark_and_light_selection() {
    let app = make_test_app().await;
    let id = ThreadId::from_u128(10);
    let mut evidence = Vec::new();
    for (background, foreground) in [
        ((20, 20, 20), (230, 230, 230)),
        ((250, 250, 250), (20, 20, 20)),
    ] {
        for (status, expected) in [
            (ThreadStatus::Idle, ratatui::style::Color::Green),
            (
                ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
                ratatui::style::Color::Cyan,
            ),
            (ThreadStatus::SystemError, ratatui::style::Color::Red),
        ] {
            let view = app.agents_overview_view(
                vec![overview_thread(id, None, "Selected task", status)],
                Some(id),
            );
            crate::terminal_palette::with_test_default_colors(
                crate::terminal_probe::DefaultColors {
                    fg: foreground,
                    bg: background,
                },
                || {
                    let area = Rect::new(0, 0, 80, 12);
                    let mut buffer = ratatui::buffer::Buffer::empty(area);
                    view.render(area, &mut buffer);
                    let row = buffer
                        .content
                        .chunks(80)
                        .find(|row| row.iter().any(|cell| cell.symbol() == "›"))
                        .unwrap();
                    let marker = row.iter().position(|cell| cell.symbol() == "›").unwrap();
                    assert_eq!(row[marker + 2].fg, expected);
                    evidence.push(format!(
                        "{background:?} | {} | {:?}",
                        row[marker + 2].symbol(),
                        row[marker + 2].style()
                    ));
                },
            );
        }
    }
    insta::assert_snapshot!(evidence.join("\n"));
}
