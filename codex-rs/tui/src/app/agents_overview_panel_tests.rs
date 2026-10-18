//! Detail hierarchy, responsive wrapping, and semantic colors on the actual widget.

use super::*;
use codex_app_server_protocol::TeamControlState;
use codex_app_server_protocol::TeamExecutionMode;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;

#[tokio::test]
async fn held_task_details_prioritize_status_and_wrap_at_both_panel_widths() {
    let mut app = make_test_app().await;
    let id = ThreadId::from_u128(/*value*/ 42);
    app.agents_hosts.manifest = Some("manifest.toml".into());
    let host = app.agents_hosts.foreground.clone();
    let mut thread = overview_thread(id, None, "Review the parser", ThreadStatus::Idle);
    thread.updated_at = 4_000_000_000;
    thread.cwd = codex_utils_path_uri::LegacyAppPathString::from_string(
        "/workspace/project-with-a-long-name/components/parser",
    );
    thread.model = Some("gpt-6.1-sol".into());
    thread.reasoning_effort = Some(codex_protocol::openai_models::ReasoningEffort::Medium);
    let agent = serde_json::from_value(serde_json::json!({
        "enrollment": {
            "address": { "machineId": host.machine_id, "serverId": host.server_id, "threadId": id.to_string() },
            "alias": "Reviewer", "manuallyHeld": false, "continuationTurnId": "interrupted-turn"
        },
        "runtime": null, "status": "idle", "currentModel": null, "currentEffort": null,
        "nextModel": "gpt-6.1-sol", "nextEffort": "medium", "project": null,
        "discovery": {
            "availability": {
                "synchronization": "current", "activity": "idle", "readiness": "held",
                "holds": ["stopped", "continuation"],
                "reasons": ["Team is stopped", "An interrupted turn has priority over new work"]
            },
            "environments": [{
                "id": "local", "current": false, "next": true,
                "os": "linux", "shell": "bash", "declaredCapabilities": ["native-tests", "code-review"]
            }]
        }
    })).unwrap();
    app.agents_hosts.snapshot = Some(codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 8,
            mode: TeamExecutionMode::Stopped,
        },
        servers: Vec::new(),
        agents: vec![agent],
        messages: Vec::new(),
        board: Default::default(),
        rejected_outbox: Vec::new(),
    });

    // The scoped palette makes semantic colors deterministic without changing process state.
    crate::terminal_palette::with_test_default_colors(
        crate::terminal_probe::DefaultColors {
            fg: (220, 220, 220),
            bg: (16, 16, 16),
        },
        || {
            let mut view = app.agents_overview_view(vec![thread], Some(id));

            // Search indexes metadata independently of which detail lines are visible.
            for (query, expected) in [
                ("gpt-6.1-sol", Some(0)),
                ("medium", Some(0)),
                ("native-tests", Some(0)),
                ("gpt-6-astra", Some(usize::MAX)),
            ] {
                view.handle_key_event(KeyCode::Char('/').into());
                view.handle_paste(query.into());
                assert_eq!(view.selected_index(), expected);
                view.handle_key_event(KeyCode::Esc.into());
            }
            let mut snapshots = Vec::new();
            for (width, height) in [(100, 40), (160, 40), (100, 24), (100, 18)] {
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                view.render(area, &mut buffer);
                let details = buffer
                    .content
                    .chunks(usize::from(width))
                    .filter_map(|row| {
                        let divider = row.iter().position(|cell| cell.symbol() == "│")?;
                        Some(
                            row[divider + 1..]
                                .iter()
                                .map(ratatui::buffer::Cell::symbol)
                                .collect::<String>()
                                .trim()
                                .to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(details.contains("Held"));
                assert!(details.contains("Team is stopped"));
                assert_eq!(details.matches("gpt-6.1-sol").count(), 1);
                if height == 40 {
                    assert_eq!(details.lines().filter(|line| *line == "Project").count(), 1);
                    assert!(details.find("Held").unwrap() < details.find("Project").unwrap());
                    assert!(details.contains("native-tests"));
                } else {
                    assert!(
                        details.ends_with('…'),
                        "clipped details must advertise overflow: {details}"
                    );
                }
                if height == 24 {
                    assert!(details.contains("Project"));
                    assert!(details.contains("/workspace/"));
                }

                for (text, color) in [("Held", Color::Yellow), ("Current", Color::Green)] {
                    let cell = buffer
                        .content
                        .chunks(usize::from(width))
                        .find_map(|row| {
                            let line = row
                                .iter()
                                .map(ratatui::buffer::Cell::symbol)
                                .collect::<String>();
                            let byte = line.find(text)?;
                            Some(&row[line[..byte].chars().count()])
                        })
                        .expect("status must remain visible");
                    assert_eq!(cell.fg, color);
                    assert!(cell.modifier.contains(Modifier::BOLD));
                }
                snapshots.push(format!("{width}x{height}\n{details}"));
            }
            insta::assert_snapshot!(snapshots.join("\n---\n"));
        },
    );
}
