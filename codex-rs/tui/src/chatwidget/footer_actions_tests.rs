//! Click replay opens Rewind without stealing drag-to-copy or changing composer contents.

use super::*;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::rendered_selection::RegionAction;
use crossterm::event::MouseButton::Left;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn rewind_footer_stationary_click_emits_action_but_drag_does_not() {
    let (mut widget, _, mut events, mut operations) = make_chatwidget_manual_with_sender().await;
    widget.set_rewind_available(/*available*/ true);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 8,
    );
    let mut buffer = Buffer::empty(area);
    widget
        .bottom_pane_renderable(crate::bottom_pane::ComposerRenderOptions {
            command_popup_placement: crate::bottom_pane::CommandPopupPlacement::Overlay,
            ..Default::default()
        })
        .render(area, &mut buffer);
    let position = area
        .positions()
        .find(|position| buffer[*position].symbol() == "[")
        .expect("Rewind control");
    while events.try_recv().is_ok() {}
    while operations.try_recv().is_ok() {}
    let mouse = |kind, column| {
        TuiEvent::Mouse(MouseEvent {
            kind,
            column,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        })
    };

    // Use another cell of the same control for the independent click. Repeating the drag's
    // starting cell immediately is a double-click, which correctly selects a word instead.
    for gesture in [
        vec![
            mouse(MouseEventKind::Down(Left), position.x),
            mouse(MouseEventKind::Drag(Left), position.x + 3),
            mouse(MouseEventKind::Up(Left), position.x),
        ],
        vec![
            mouse(MouseEventKind::Down(Left), position.x + 1),
            mouse(MouseEventKind::Up(Left), position.x + 1),
        ],
    ] {
        let dragged = gesture.len() == 3;
        let mut clicks = 0;
        for event in gesture {
            let action = widget
                .rendered_selection
                .borrow_mut()
                .handle_event(&event, &buffer);
            if let Some(RegionAction::Click(mouse)) = action {
                clicks += 1;
                assert!(widget.handle_footer_event(&TuiEvent::Mouse(mouse), &[]));
            }
        }
        assert_eq!(clicks, usize::from(!dragged));
        let actions = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| matches!(event, AppEvent::OpenRewind))
            .count();
        assert_eq!(actions, usize::from(!dragged));
        assert!(operations.try_recv().is_err());
        assert!(widget.composer_is_empty());
    }

    // A modal owns its backdrop; closing it cannot resurrect the old button before repaint.
    widget.open_warnings(&[]);
    widget
        .bottom_pane_renderable(crate::bottom_pane::ComposerRenderOptions {
            command_popup_placement: crate::bottom_pane::CommandPopupPlacement::Overlay,
            ..Default::default()
        })
        .render(area, &mut buffer);
    assert!(!widget.handle_footer_event(&mouse(MouseEventKind::Down(Left), position.x), &[]));
    widget.handle_key_event(KeyCode::Esc.into());
    assert!(!widget.handle_footer_event(&mouse(MouseEventKind::Down(Left), position.x), &[]));

    // A state change before the next frame must not revive the previous clickable rectangle.
    widget.apply_external_edit("keep draft".into());
    assert!(!widget.handle_footer_event(&mouse(MouseEventKind::Down(Left), position.x), &[]));
    assert_eq!(widget.bottom_pane.composer_text(), "keep draft");
    widget.clear_footer_hit_targets();
    assert!(!widget.handle_footer_event(&mouse(MouseEventKind::Down(Left), position.x), &[]));

    widget.apply_external_edit(String::new());
    assert!(widget.rewind_draft_is_empty());
    widget.apply_external_edit("pending".into());
    widget
        .bottom_pane
        .set_composer_pending_pastes(vec![("pending".into(), "paste".into())]);
    assert!(!widget.rewind_draft_is_empty());
    assert_eq!(
        widget.bottom_pane.composer_pending_pastes(),
        vec![("pending".into(), "paste".into())]
    );
}
