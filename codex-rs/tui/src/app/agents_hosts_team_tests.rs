use super::*;
use pretty_assertions::assert_eq;

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
