//! Responsive paint and hit testing share one layout; active input always retains its hints.

use super::*;
use pretty_assertions::assert_eq;

fn composer() -> ChatComposer {
    let (mut composer, _) = super::super::tests::new_test_composer();
    let mut keymap = RuntimeKeymap::defaults();
    keymap.app.open_transcript = vec![key_hint::plain(KeyCode::F(3))];
    keymap.app.open_warnings = vec![key_hint::plain(KeyCode::F(2))];
    composer.set_keymap_bindings(&keymap);
    composer.set_status_line_enabled(/*enabled*/ true);
    composer.set_status_line(Some(Line::from("MODEL STATUS")));
    composer.set_rewind_available(/*available*/ true);
    composer.set_agent_navigation_available(/*available*/ true);
    composer
}

fn render(composer: &ChatComposer, width: u16, options: ComposerRenderOptions<'_>) -> Buffer {
    let options = composer.resolve_render_options(options);
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        composer.desired_height_with_options(width, options),
    );
    let mut buffer = Buffer::empty(area);
    composer.render_with_options(area, &mut buffer, /*mask_char*/ None, options);
    buffer
}

fn text(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn rewind_footer_layouts_preserve_status_and_complete_shortcuts() {
    let composer = composer();
    let mut frames = Vec::new();
    for (width, warnings) in [
        (90, 0),
        (60, 0),
        (48, 0),
        (35, 0),
        (19, 0),
        (12, 0),
        (8, 0),
        (90, 3),
        (48, 3),
        (19, 3),
    ] {
        let options = ComposerRenderOptions {
            separate_status_line: true,
            warning_count: warnings,
            ..Default::default()
        };
        let buffer = render(&composer, width, options);
        let hit = composer.footer.rewind_area.get();
        if let Some(hit) = hit {
            let painted = (hit.x..hit.right())
                .map(|x| buffer[(x, hit.y)].symbol())
                .collect::<String>();
            assert!(
                painted == "[Rewind…]" || painted == "/rewind",
                "{painted:?}"
            );
            assert!(composer.rewind_contains(hit.as_position()));
            if let Some(warning) = composer.footer.warning_notice_area.get() {
                assert!(!hit.intersects(warning));
            }
        }
        frames.push(format!(
            "width {width}, warnings {warnings}, hit {hit:?}\n{}",
            text(&buffer)
        ));
    }
    insta::assert_snapshot!("rewind_footer_layouts", frames.join("\n\n"));
}

#[test]
fn rewind_footer_disables_for_draft_attachments_pending_paste_and_unavailable_history() {
    let mut frames = Vec::new();
    for setup in [
        |composer: &mut ChatComposer| {
            composer.set_text_content("keep draft".into(), Vec::new(), Vec::new())
        },
        |composer: &mut ChatComposer| {
            composer.set_remote_image_urls(vec!["https://example.com/image.png".into()])
        },
        |composer: &mut ChatComposer| {
            composer
                .draft
                .pending_pastes
                .push(("pending".into(), "paste".into()))
        },
        |composer: &mut ChatComposer| composer.set_rewind_available(/*available*/ false),
        ChatComposer::set_parent_owned_thread,
        |composer: &mut ChatComposer| composer.set_team_execution_held(/*held*/ true),
    ] {
        let mut composer = composer();
        setup(&mut composer);
        let before = (
            composer.current_text(),
            composer.draft.pending_pastes.clone(),
            composer.attachments.remote_image_urls(),
        );
        let buffer = render(
            &composer,
            /*width*/ 80,
            ComposerRenderOptions {
                separate_status_line: true,
                ..Default::default()
            },
        );
        assert!(composer.footer.rewind_area.get().is_none());
        assert_eq!(
            (
                composer.current_text(),
                composer.draft.pending_pastes.clone(),
                composer.attachments.remote_image_urls()
            ),
            before
        );
        let label = buffer
            .area
            .positions()
            .find(|position| {
                position.y == buffer.area.bottom() - 1 && buffer[*position].symbol() == "["
            })
            .expect("disabled label remains discoverable");
        assert_eq!(
            buffer[label].fg,
            crate::style::secondary_text_style()
                .fg
                .unwrap_or(ratatui::style::Color::Reset)
        );
        frames.push(text(&buffer));
    }
    insta::assert_snapshot!("rewind_footer_disabled", frames.join("\n\n"));
}

#[test]
fn rewind_footer_rechecks_parent_and_team_restrictions_before_repaint() {
    for setup in [
        ChatComposer::set_parent_owned_thread,
        |composer: &mut ChatComposer| composer.set_team_execution_held(/*held*/ true),
    ] {
        let mut composer = composer();
        render(
            &composer,
            /*width*/ 80,
            ComposerRenderOptions {
                separate_status_line: true,
                ..Default::default()
            },
        );
        let position = composer
            .footer
            .rewind_area
            .get()
            .expect("enabled control")
            .as_position();
        setup(&mut composer);
        assert!(!composer.rewind_contains(position));
    }
}

#[test]
fn rewind_footer_hides_and_clears_stale_targets_for_active_hints_and_resize() {
    for setup in [
        |composer: &mut ChatComposer| {
            composer.begin_history_search();
        },
        |composer: &mut ChatComposer| {
            composer.set_footer_hint_override(Some(vec![("ctrl+x".into(), "pending chord".into())]))
        },
        |composer: &mut ChatComposer| composer.set_task_running(/*running*/ true),
        |composer: &mut ChatComposer| {
            composer.set_text_content("queued draft".into(), Vec::new(), Vec::new());
            composer.set_task_running(/*running*/ true);
        },
        |composer: &mut ChatComposer| {
            composer.insert_str("/");
        },
    ] {
        let mut composer = composer();
        let options = ComposerRenderOptions {
            separate_status_line: true,
            ..Default::default()
        };
        render(&composer, /*width*/ 80, options);
        let previous = composer.footer.rewind_area.get().expect("initial target");
        setup(&mut composer);
        let buffer = render(&composer, /*width*/ 80, options);
        assert!(!text(&buffer).contains("[Rewind…]"));
        assert!(!composer.rewind_contains(previous.as_position()));
        assert_eq!(composer.footer.rewind_area.get(), None);
    }
    let composer = composer();
    let options = ComposerRenderOptions {
        separate_status_line: true,
        ..Default::default()
    };
    render(&composer, /*width*/ 80, options);
    let previous = composer.footer.rewind_area.get().expect("initial target");
    render(&composer, /*width*/ 8, options);
    assert!(!composer.rewind_contains(previous.as_position()));
    assert_eq!(composer.footer.rewind_area.get(), None);

    let footer = TranscriptFooter {
        text: "Find: needle".into(),
        cursor_column: Some(6),
        is_interactive: true,
    };
    let buffer = render(
        &composer,
        /*width*/ 80,
        ComposerRenderOptions {
            footer: Some(&footer),
            ..options
        },
    );
    assert!(text(&buffer).contains("Find: needle"));
    assert_eq!(composer.footer.rewind_area.get(), None);
}

#[test]
fn rewind_footer_does_not_replace_inline_configured_status_or_add_height() {
    let mut composer = composer();
    let options = ComposerRenderOptions::default();
    let enabled = render(&composer, /*width*/ 80, options);
    composer.footer.rewind_available = None;
    assert_eq!(render(&composer, /*width*/ 80, options), enabled);
    let options = ComposerRenderOptions {
        separate_status_line: true,
        ..options
    };
    let height = composer.desired_height_with_options(/*width*/ 80, options);
    composer.set_rewind_available(/*available*/ true);
    assert_eq!(
        composer.desired_height_with_options(/*width*/ 80, options),
        height
    );

    composer.set_status_line_enabled(/*enabled*/ false);
    let buffer = render(
        &composer,
        /*width*/ 80,
        ComposerRenderOptions::default(),
    );
    assert!(text(&buffer).contains("/rewind"));
    assert!(!text(&buffer).contains("[Rewind…]"));
    assert_eq!(composer.footer.rewind_area.get(), None);
}
