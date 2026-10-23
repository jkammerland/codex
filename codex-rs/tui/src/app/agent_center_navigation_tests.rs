//! Back, cancellation and focus remain local to the command center.

use super::*;
use crate::bottom_pane::CancellationEvent;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn overview_cancel_back_and_quit_have_distinct_outcomes() {
    for required in [false, true] {
        let mut app = make_test_app().await;
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        app.app_event_tx = AppEventSender::new(tx);
        let mut view = app.agents_overview_view(Vec::new(), None);
        app.agents_overview
            .view_state
            .lock()
            .unwrap()
            .overview_required = required;

        view.handle_key_event(KeyCode::Char('/').into());
        view.handle_paste("query".into());
        assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
        assert!(
            !app.agents_overview
                .view_state
                .lock()
                .unwrap()
                .editing_metadata()
        );
        assert!(!view.is_complete());

        view.handle_key_event(KeyCode::Char('?').into());
        assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
        assert!(!screen(&view, 80, 24).contains("Task shortcuts"));
        assert_eq!(view.on_ctrl_c(), CancellationEvent::Handled);
        assert!(events.try_recv().is_err());

        view.handle_key_event(KeyCode::Esc.into());
        assert_eq!(view.is_complete(), !required);
        assert!(events.try_recv().is_err());

        let mut view = app.agents_overview_view(Vec::new(), None);
        view.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(events.try_recv(), Ok(AppEvent::Exit(_))));
    }
}

#[tokio::test]
async fn overview_tab_focus_preserves_results_and_search_owns_browser_letters() {
    let app = make_test_app().await;
    let ready = ThreadId::from_u128(42);
    let needs_you = ThreadId::from_u128(43);
    let mut view = app.agents_overview_view(
        vec![
            overview_thread(ready, None, "Ready", ThreadStatus::Idle),
            overview_thread(needs_you, None, "Needs input", ThreadStatus::SystemError),
        ],
        Some(ready),
    );

    view.handle_key_event(KeyCode::Tab.into());
    assert_eq!(view.rows[view.selected_index().unwrap()].thread_id, ready);
    view.handle_key_event(KeyCode::Down.into());
    assert_eq!(
        view.rows[view.selected_index().unwrap()].thread_id,
        needs_you
    );
    view.handle_key_event(KeyCode::Enter.into());
    assert!(!view.is_complete());

    view.handle_key_event(KeyCode::Char('/').into());
    view.handle_paste("hlq".into());
    assert!(screen(&view, 80, 24).contains("Search › hlq"));
    view.handle_key_event(KeyCode::Esc.into());
    assert!(!view.is_complete());
    view.handle_key_event(KeyCode::Char('h').into());
    assert!(view.is_complete());
}

#[tokio::test]
async fn host_focus_filters_equal_thread_ids_by_complete_host_identity() {
    use crate::app::agents_hosts::HostKey;
    use crate::app::agents_overview_view::command_center::hosts::HostSummary;

    let app = make_test_app().await;
    let id = ThreadId::from_u128(42);
    let mut view = app.agents_overview_view(
        vec![
            overview_thread(id, None, "First server", ThreadStatus::Idle),
            overview_thread(id, None, "Second server", ThreadStatus::Idle),
        ],
        None,
    );
    let first = HostKey {
        machine_id: "same-machine".into(),
        server_id: "first".into(),
    };
    let second = HostKey {
        machine_id: "same-machine".into(),
        server_id: "second".into(),
    };
    view.rows[0].host = Some(first.clone());
    view.rows[1].host = Some(second.clone());
    {
        let mut state = app.agents_overview.view_state.lock().unwrap();
        state.hosts = vec![
            HostSummary {
                key: first,
                label: "First".into(),
                connected: true,
            },
            HostSummary {
                key: second.clone(),
                label: "Second".into(),
                connected: false,
            },
        ];
        state.hosts_visible = true;
    }

    view.handle_key_event(KeyCode::Tab.into());
    view.handle_key_event(KeyCode::Down.into());
    view.handle_key_event(KeyCode::Down.into());
    assert_eq!(view.rows[view.selected_index().unwrap()].host, Some(second));
    let output = screen(&view, 140, 24);
    assert!(output.contains("Second server"));
    assert!(!output.contains("First server"));
    let narrow = screen(&view, 80, 24);
    assert!(narrow.contains("esc all hosts"));
    insta::assert_snapshot!("host_scope_remains_reachable_after_resize", narrow);
    view.handle_key_event(KeyCode::Esc.into());
    assert!(!view.is_complete());
    assert!(
        app.agents_overview
            .view_state
            .lock()
            .unwrap()
            .host_filter
            .is_none()
    );
    assert!(!app.agents_overview.view_state.lock().unwrap().hosts_focused);
    let restored = screen(&view, 80, 24);
    assert!(restored.contains("First server"));
    assert!(restored.contains("Second server"));
}
