use super::*;
use codex_app_server_protocol::ThreadAgentBarrier;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;

#[test]
fn agent_work_details_distinguish_unknown_from_empty() {
    assert!(detail_lines(None).is_empty());
    let work = ThreadAgentWork {
        revision: 1,
        pending_mail: 0,
        waiting_tasks: None,
    };
    pretty_assertions::assert_eq!(
        detail_lines(Some(&work))[0].to_string(),
        "Mail      0 queued"
    );
}

#[test]
fn agent_work_details_wrap_in_narrow_panel() {
    assert_details_snapshot(/*width*/ 28);
}

#[test]
fn agent_work_details_wrap_in_wide_panel() {
    assert_details_snapshot(/*width*/ 65);
}

fn assert_details_snapshot(width: u16) {
    let work = ThreadAgentWork {
        revision: 9,
        pending_mail: 2,
        waiting_tasks: Some(ThreadAgentBarrier {
            total: 3,
            remaining: 2,
            held: 1,
            unavailable: 1,
        }),
    };
    let mut terminal = Terminal::new(TestBackend::new(width, 7)).expect("terminal");
    terminal
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(detail_lines(Some(&work))).wrap(Wrap { trim: false }),
                frame.area(),
            );
        })
        .expect("render");
    insta::assert_snapshot!(format!("agent_work_details_{width}"), terminal.backend());
}

#[tokio::test]
async fn agent_work_updates_preserve_newer_counts_on_both_host_views() {
    use crate::app::agents_hosts::state;
    use crate::app::agents_overview::tests::overview_thread;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ThreadAgentWorkUpdatedNotification;
    use codex_app_server_protocol::ThreadClosedNotification;
    use codex_app_server_protocol::ThreadStatus;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;

    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(/*value*/ 7);
    let original = overview_thread(
        id,
        /*parent_thread_id*/ None,
        "Worker",
        ThreadStatus::Idle,
    );
    app.agents_overview
        .threads
        .insert(id, Some(original.clone()));
    let mut remote = crate::app::agents_overview::AgentsOverviewState::default();
    remote.threads.insert(id, Some(original));

    // A delayed notification must not replace a later accepted snapshot.
    for (revision, pending_mail) in [(9, 2), (8, 3), (10, 0)] {
        let update =
            ServerNotification::ThreadAgentWorkUpdated(ThreadAgentWorkUpdatedNotification {
                thread_id: id.to_string(),
                agent_work: Some(ThreadAgentWork {
                    revision,
                    pending_mail,
                    waiting_tasks: None,
                }),
            });
        app.track_agents_overview_notification(&update);
        state::track_notification(&mut remote, &update);
        let expected = if revision == 8 {
            ThreadAgentWork {
                revision: 9,
                pending_mail: 2,
                waiting_tasks: None,
            }
        } else {
            ThreadAgentWork {
                revision,
                pending_mail,
                waiting_tasks: None,
            }
        };
        assert_eq!(
            app.agents_overview.threads[&id]
                .as_ref()
                .unwrap()
                .agent_work,
            Some(expected.clone())
        );
        assert_eq!(
            remote.threads[&id].as_ref().unwrap().agent_work,
            Some(expected)
        );
    }
    assert!(app.agents_overview.refresh_thread_ids.is_empty());

    // Closing drops the old controller's revision, so a reopened controller can
    // publish its own authoritative snapshot beginning at a lower revision.
    let closed = ServerNotification::ThreadClosed(ThreadClosedNotification {
        thread_id: id.to_string(),
    });
    app.track_agents_overview_notification(&closed);
    state::track_notification(&mut remote, &closed);
    assert_eq!(
        app.agents_overview.threads[&id]
            .as_ref()
            .unwrap()
            .agent_work,
        None
    );
    assert_eq!(remote.threads[&id].as_ref().unwrap().agent_work, None);
    let reopened = ThreadAgentWork {
        revision: 1,
        pending_mail: 4,
        waiting_tasks: None,
    };
    let update = ServerNotification::ThreadAgentWorkUpdated(ThreadAgentWorkUpdatedNotification {
        thread_id: id.to_string(),
        agent_work: Some(reopened.clone()),
    });
    app.track_agents_overview_notification(&update);
    state::track_notification(&mut remote, &update);
    assert_eq!(
        app.agents_overview.threads[&id]
            .as_ref()
            .unwrap()
            .agent_work,
        Some(reopened.clone())
    );
    assert_eq!(
        remote.threads[&id].as_ref().unwrap().agent_work,
        Some(reopened)
    );
}

#[tokio::test]
async fn agent_work_is_visible_in_the_actual_task_details_panel() {
    use crate::app::agents_overview::tests::overview_thread;
    use crate::render::renderable::Renderable;
    use codex_app_server_protocol::ThreadStatus;
    use codex_protocol::ThreadId;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let (app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let id = ThreadId::from_u128(/*value*/ 7);
    let mut thread = overview_thread(
        id,
        /*parent_thread_id*/ None,
        "Review both results",
        ThreadStatus::Idle,
    );
    thread.cwd = codex_utils_path_uri::LegacyAppPathString::from_string("/workspace/project");
    thread.agent_work = Some(ThreadAgentWork {
        revision: 9,
        pending_mail: 2,
        waiting_tasks: Some(ThreadAgentBarrier {
            total: 3,
            remaining: 2,
            held: 1,
            unavailable: 1,
        }),
    });
    let view = app.agents_overview_view(vec![thread], Some(id));
    let mut snapshots = Vec::new();
    for width in [100, 160] {
        let area = Rect::new(0, 0, width, 30);
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
                        .trim_end()
                        .to_owned(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains("2 queued"), "{details}");
        assert!(details.contains("Barrier"), "{details}");
        snapshots.push(format!("{width} columns\n{details}"));
    }
    insta::assert_snapshot!(snapshots.join("\n---\n"));
}
