use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn enrollment_error_keeps_server_explanation_and_original_retry_identity() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    let reason = "Session was not found on workstation/private. Enroll it on its owning server.";
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/rpc"))
        .respond_with(
            wiremock::ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": reason})),
        )
        .expect(2)
        .mount(&server)
        .await;
    let (mut app, mut events, _) = crate::app::tests::make_test_app_with_channels().await;
    app.agents_hosts.client = Some(
        codex_team::TeamClient::new(&server.uri(), "test-token".into())
            .expect("valid isolated coordinator URL"),
    );
    app.submit_agents_team_request(TeamRequest::Enroll {
        address: codex_app_server_protocol::TeamAddress {
            machine_id: "workstation".into(),
            server_id: "private".into(),
            thread_id: ThreadId::from_u128(42).to_string(),
        },
        alias: "Reviewer".into(),
    });
    let operation_id = app
        .agents_hosts
        .team_operations
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut session = AppServerSession::discovery_placeholder();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let overview = app.agents_overview_view(Vec::new(), None);
    app.chat_widget.show_bottom_pane_view(Box::new(overview));
    let mut error_screen = String::new();

    for attempt in 0..2 {
        if attempt == 1 {
            app.retry_agents_team_request(operation_id.clone());
        }
        let update = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("operation sends a completion event");
                if let AppEvent::AgentsHostUpdate(update) = event
                    && matches!(&*update, HostUpdate::TeamOperationFinished { .. })
                {
                    break update;
                }
            }
        })
        .await?;
        app.handle_agents_host_update(&mut tui, &mut session, *update)
            .await?;
        let notice = app.agents_hosts.notice.as_deref().unwrap();
        assert!(notice.contains(reason));
        assert!(notice.contains("outcome is unknown"));
        assert!(notice.contains("retry with the same operation ID"));
        assert!(
            app.agents_hosts
                .failed_team_operations
                .contains(&operation_id)
        );
        assert!(app.agents_hosts.team_operations.contains_key(&operation_id));
        let screen = crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 90);
        assert!(screen.contains("Team action needs attention"));
        // Wrapping may split the server address at its slash; retain all of its text.
        let unwrapped: String = screen.split_whitespace().collect();
        assert!(unwrapped.contains("workstation/private"), "{screen}");
        assert!(unwrapped.contains("owningserver"), "{screen}");
        error_screen = screen.replace(&operation_id, "operation-id");
        app.chat_widget
            .handle_key_event_with_activation_key(KeyCode::Esc.into(), None);
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    let operation: codex_team::TeamOperation = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(operation.operation_id, operation_id);
    insta::assert_snapshot!("agents_team_enrollment_error", error_screen);
    Ok(())
}

#[test]
fn team_request_editor_wraps_body_and_renders_the_edit_cursor() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut input = TextArea::new();
    input.insert_str("Review the proposed change on your machine and reply with the test result.");
    let view = ComposeView {
        title: "Request · deliver at the next turn".into(),
        input,
        input_state: RefCell::default(),
        kind: TeamCompose::Topic,
        sender: AppEventSender::new(events),
        completion: None,
    };
    let area = Rect::new(0, 0, 42, 7);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    let output = buffer
        .content
        .chunks(42)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("agents_team_request_editor", output);
    assert_eq!(view.cursor_pos(area), Some((31, 2)));
}

#[test]
fn member_checkout_keeps_the_native_path_until_host_validation() {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let draft = super::super::agents_member::MemberDraft {
        request: codex_team::TeamMemberCreateRequest {
            machine_id: "windows".into(),
            server_id: "primary".into(),
            alias: "Builder".into(),
            cwd: String::new(),
            model: None,
            effort: None,
        },
        host_label: "Windows / primary".into(),
    };
    let mut input = TextArea::new();
    input.insert_str(r"C:\repos\native project");
    let mut view = ComposeView {
        title: "Create Builder · absolute checkout path on Windows / primary".into(),
        input,
        input_state: RefCell::default(),
        kind: TeamCompose::MemberCheckout(draft),
        sender: AppEventSender::new(events),
        completion: None,
    };
    view.handle_key_event(KeyCode::Enter.into());
    let AppEvent::AgentsMember(action) = receiver.try_recv().unwrap() else {
        panic!("expected checkout completion")
    };
    let super::super::agents_member::MemberAction::LoadModels(draft) = *action else {
        panic!("expected model catalog step")
    };
    assert_eq!(draft.request.cwd, r"C:\repos\native project");
    assert_eq!(draft.request.machine_id, "windows");
    assert!(receiver.try_recv().is_err());
}
