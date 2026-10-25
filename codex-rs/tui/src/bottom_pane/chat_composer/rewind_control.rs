//! Rewind shares the passive hint row; active input hints and configured status keep priority.
//! Complete candidates determine both painted text and its one clickable control.

use super::*;

pub(super) struct RewindControlLayout {
    pub(super) line: Line<'static>,
    pub(super) hit_area: Option<Rect>,
}

impl ChatComposer {
    pub(crate) fn set_rewind_available(&mut self, available: bool) {
        self.footer.rewind_available = Some(available);
        if !available {
            self.footer.rewind_area.set(None);
        }
    }

    pub(crate) fn set_agent_navigation_available(&mut self, available: bool) {
        self.footer.agent_navigation_available = available;
    }

    pub(crate) fn clear_footer_hit_targets(&self) {
        self.footer.rewind_area.set(None);
        self.footer.warning_notice_area.set(None);
    }

    pub(crate) fn rewind_contains(&self, position: ratatui::layout::Position) -> bool {
        self.rewind_enabled()
            && self.footer.hint_override.is_none()
            && !self.footer.flash_visible()
            && !self.popup_active()
            && self.footer_mode() == FooterMode::ComposerEmpty
            && self
                .footer
                .rewind_area
                .get()
                .is_some_and(|area| area.contains(position))
    }

    fn rewind_enabled(&self) -> bool {
        self.footer.rewind_available == Some(true)
            && self.is_empty()
            && self.draft.pending_pastes.is_empty()
            && !self.is_in_paste_burst()
            && !self.is_task_running
    }

    pub(super) fn rewind_control_layout(
        &self,
        mut area: Rect,
        options: ComposerRenderOptions<'_>,
        warning_area: Option<Rect>,
    ) -> Option<RewindControlLayout> {
        // Inline status-only layouts have no spare hint row. The slash popup remains their
        // command affordance; never remove configured status fields or add another row.
        if self.footer.rewind_available.is_none()
            || (!options.separate_status_line && self.footer.status_line_enabled)
            || options.footer.is_some()
            || !self.slash_commands_enabled()
            || self.is_task_running
            || !super::super::footer::shows_passive_footer_line(&self.footer_props())
            || self.popup_active()
            || self.footer.hint_override.is_some()
            || self.footer.flash_visible()
            || self.is_in_paste_burst()
            || area.is_empty()
        {
            return None;
        }
        if let Some(warning_area) = warning_area {
            area.width = warning_area
                .x
                .saturating_sub(area.x)
                .saturating_sub(/*rhs*/ 2);
        }
        let enabled = self.rewind_enabled();
        let agents = (self.footer.agent_navigation_available && self.is_empty())
            .then(|| hint(key_hint::plain(KeyCode::Esc).into(), " Agents"));
        let details = self
            .footer
            .show_transcript_key
            .map(|key| hint(key, " Details"));
        let help = self
            .footer
            .toggle_shortcuts_key
            .map(|key| hint(key, " Help"));
        let label_style = if enabled {
            crate::style::accent_style()
        } else {
            crate::style::secondary_text_style()
        };
        let label = Span::styled("[Rewind…]", label_style);
        let mut full = Line::from(vec![label, " ".into()]);
        full.extend(key_hint::key_label_spans("/rewind"));
        let compact = Line::from(key_hint::key_label_spans("/rewind"));
        // Inline terminals retain native mouse behavior: advertise only the local command.
        let (full, full_control_width) = if options.separate_status_line {
            (full, 9)
        } else {
            (compact.clone(), 7)
        };

        // Drop secondary hints as whole units, then compact Rewind; never clip a shortcut.
        for (agents, rewind, details, help, control_width) in [
            (
                agents.as_ref(),
                &full,
                details.as_ref(),
                help.as_ref(),
                full_control_width,
            ),
            (
                agents.as_ref(),
                &full,
                None,
                help.as_ref(),
                full_control_width,
            ),
            (None, &full, None, help.as_ref(), full_control_width),
            (None, &compact, None, help.as_ref(), 7),
            (None, &compact, None, None, 7),
        ] {
            let mut line = Line::default().style(crate::style::secondary_text_style());
            if let Some(agents) = agents {
                line.extend(agents.spans.clone());
                line.push_span(" · ");
            }
            let control_x = area.x + line.width() as u16;
            line.extend(rewind.spans.clone());
            for hint in [details, help].into_iter().flatten() {
                line.push_span(" · ");
                line.extend(hint.spans.clone());
            }
            if line.width() <= usize::from(area.width) {
                return Some(RewindControlLayout {
                    line,
                    hit_area: (enabled && options.separate_status_line)
                        .then(|| Rect::new(control_x, area.y, control_width, /*height*/ 1)),
                });
            }
        }
        Some(RewindControlLayout {
            line: Line::default(),
            hit_area: None,
        })
    }
}

fn hint(key: ShortcutHint, description: &'static str) -> Line<'static> {
    let mut line = Line::from(key.spans());
    line.push_span(description);
    line
}
