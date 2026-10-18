//! Status filters and viewport state survive refreshes of the shared task projection.

pub(crate) mod board;
mod hints;
mod input;
mod navigation;
mod render;
mod rows;

use super::*;

/// Authoritative team control state shared by Tasks and Board presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TeamControlSummary {
    pub(crate) mode: codex_app_server_protocol::TeamExecutionMode,
    pub(crate) applied: usize,
    pub(crate) total: usize,
}

impl From<&codex_team::TeamSnapshot> for TeamControlSummary {
    fn from(snapshot: &codex_team::TeamSnapshot) -> Self {
        Self {
            mode: snapshot.control.mode,
            applied: snapshot
                .servers
                .iter()
                .filter(|server| server.applied_revision == Some(snapshot.control.revision))
                .count(),
            total: snapshot.servers.len(),
        }
    }
}

impl std::fmt::Display for TeamControlSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Team {:?} · {}/{} hosts applied",
            self.mode, self.applied, self.total
        )
    }
}

// Counts and filtering use the same status groups.
pub(super) const TASK_FILTERS: &[(&str, Option<AgentsOverviewGroup>)] = &[
    ("All", None),
    ("Needs you", Some(AgentsOverviewGroup::NeedsYou)),
    ("Working", Some(AgentsOverviewGroup::Working)),
    ("Idle", Some(AgentsOverviewGroup::Ready)),
    ("Inactive", Some(AgentsOverviewGroup::Finished)),
];

impl AgentsOverviewView {
    pub(in crate::app::agents_overview_view) fn selectable_indices(&self) -> Vec<usize> {
        let mut indices = self.visible_indices();
        if self.state().has_more {
            indices.push(usize::MAX);
        }
        indices
    }

    pub(in crate::app::agents_overview_view) fn reconcile_command_center_selection(&mut self) {
        let visible = self.selectable_indices();
        if !visible.contains(&self.selected) {
            self.selected = visible.first().copied().unwrap_or(usize::MAX);
            self.state().show_more_selected = false;
        }
    }
}
