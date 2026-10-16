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
