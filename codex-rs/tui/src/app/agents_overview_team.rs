//! Enrollment-backed task rows keep unloaded Board-only teammates discoverable.
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::style::StatusTone;
use crate::style::accent_style;
use crate::style::secondary_text_style;
use crate::style::status_style;

use super::agents_hosts::HostKey;
use super::agents_overview_details::AgentsOverviewDetails;
use super::agents_overview_view::AgentsOverviewGroup;
use super::agents_overview_view::AgentsOverviewRow;

pub(super) fn append_enrolled_rows(
    snapshot: &codex_team::TeamSnapshot,
    rows: &mut Vec<AgentsOverviewRow>,
) {
    for agent in &snapshot.agents {
        if agent.status == "archived"
            || agent
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.archived)
        {
            continue;
        }
        let address = &agent.enrollment.address;
        let host = HostKey {
            machine_id: address.machine_id.clone(),
            server_id: address.server_id.clone(),
        };
        if rows
            .iter()
            .any(|row| row.host.as_ref() == Some(&host) && row.thread.id == address.thread_id)
            || agent
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.parent_thread_id.is_some())
        {
            continue;
        }
        let Ok(thread_id) = ThreadId::from_string(&address.thread_id) else {
            continue;
        };
        let status = match agent.discovery.availability.activity {
            TeamActivity::Idle => ThreadStatus::Idle,
            TeamActivity::Working => ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            TeamActivity::WaitingForApproval => ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
            },
            TeamActivity::WaitingForInput => ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
            },
            TeamActivity::Error => ThreadStatus::SystemError,
            TeamActivity::Unknown | TeamActivity::Archived | TeamActivity::Unloaded => {
                ThreadStatus::NotLoaded
            }
        };
        rows.push(AgentsOverviewRow {
            metadata_known: false,
            machine_label: snapshot
                .servers
                .iter()
                .find(|server| {
                    server.machine_id == address.machine_id && server.server_id == address.server_id
                })
                .map(|server| server.machine_label.clone()),
            host: Some(host),
            pin_rank: None,
            pins_supported: false,
            details: AgentsOverviewDetails {
                availability: Some(agent.discovery.availability.clone()),
                lines: vec!["History metadata: unknown (enrollment only)".into()],
                environment_lines: Vec::new(),
                usage_lines: Vec::new(),
                last_message: None,
            },
            thread: Thread {
                id: address.thread_id.clone(),
                environments: None,
                extra: None,
                session_id: String::new(),
                forked_from_id: None,
                parent_thread_id: None,
                preview: String::new(),
                ephemeral: false,
                section: None,
                section_entered_at: None,
                project_id: agent.project.clone(),
                history_mode: Default::default(),
                model_provider: String::new(),
                model: agent.next_model.clone(),
                reasoning_effort: None,
                created_at: 0,
                updated_at: 0,
                recency_at: None,
                status: status.clone(),
                path: None,
                cwd: codex_utils_path_uri::LegacyAppPathString::from_string("Unknown"),
                cli_version: String::new(),
                originator: None,
                source: SessionSource::Unknown,
                agent_work: None,
                can_accept_direct_input: None,
                thread_source: None,
                agent_nickname: None,
                agent_role: None,
                git_info: None,
                name: Some(agent.enrollment.alias.clone()),
                daybreak_enabled: None,
                turns: Vec::new(),
            },
            thread_id,
            group: AgentsOverviewGroup::for_status(&status),
            is_current: false,
            has_voice: false,
        });
    }
}

#[derive(Default)]
pub(super) struct TaskDetailLines {
    pub(super) availability: Option<TeamAvailability>,
    pub(super) status: Vec<Line<'static>>,
    pub(super) environment: Vec<Line<'static>>,
}

pub(super) fn task_lines(
    row: &AgentsOverviewRow,
    snapshot: Option<&codex_team::TeamSnapshot>,
) -> TaskDetailLines {
    let Some(host) = &row.host else {
        return TaskDetailLines::default();
    };
    let agent = snapshot.and_then(|snapshot| {
        snapshot.agents.iter().find(|agent| {
            let address = &agent.enrollment.address;
            address.machine_id == host.machine_id
                && address.server_id == host.server_id
                && address.thread_id == row.thread.id
        })
    });
    let mut lines = vec![Line::default()];
    if let Some(agent) = agent {
        lines.push(Line::from(vec![
            "Team".bold(),
            " · ".into(),
            agent.enrollment.alias.clone().into(),
        ]));
        lines.extend(availability_lines(&agent.discovery.availability));
        if agent.enrollment.manually_held {
            lines.push(field(
                "Hold",
                "Manually held",
                status_style(StatusTone::Attention),
            ));
        }
    } else {
        lines.push("Connection".bold().into());
    }

    let mut server_line = field("Server", host.server_id.clone(), Style::default());
    let mut server_error = None;
    if let Some(snapshot) = snapshot
        && let Some(server) = snapshot.servers.iter().find(|server| {
            server.machine_id == host.machine_id && server.server_id == host.server_id
        })
    {
        let applied = server.applied_revision.map_or_else(
            || "Unconfirmed".into(),
            |revision| format!("applied {revision}"),
        );
        let tone = if server.connected && server.applied_revision == Some(snapshot.control.revision)
        {
            StatusTone::Success
        } else {
            StatusTone::Attention
        };
        server_line.spans.extend([
            Span::styled(" · Control  ", secondary_text_style()),
            Span::styled(applied, status_style(tone)),
        ]);
        server_error = server.error.as_ref();
    }
    lines.push(server_line);
    if let Some(error) = server_error {
        lines.push(field(
            "Error",
            error.clone(),
            status_style(StatusTone::Failure),
        ));
    }

    let mut environment = Vec::new();
    if let Some(agent) = agent {
        let execution = execution_lines(row, agent);
        if !execution.is_empty() {
            lines.extend([Line::default(), "Execution".bold().into()]);
            lines.extend(execution);
        }
        environment.extend([Line::default(), "Environment".bold().into()]);
        environment.extend(environment_lines(&agent.discovery));
    }
    TaskDetailLines {
        availability: agent.map(|agent| agent.discovery.availability.clone()),
        status: lines,
        environment,
    }
}

fn field(label: &str, value: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9} "), secondary_text_style()),
        Span::styled(value.into(), style),
    ])
}

fn availability_lines(availability: &TeamAvailability) -> Vec<Line<'static>> {
    let (readiness, readiness_style) = match availability.readiness {
        TeamReadiness::Ready => ("Ready", status_style(StatusTone::Success)),
        TeamReadiness::Resumable => ("Resumable", status_style(StatusTone::Attention)),
        TeamReadiness::Busy => ("Busy", accent_style()),
        TeamReadiness::Held => ("Held", status_style(StatusTone::Attention)),
        TeamReadiness::Unknown => ("Unknown", secondary_text_style()),
    };
    let (activity, activity_style) = match availability.activity {
        TeamActivity::Idle => ("Idle", Style::default()),
        TeamActivity::Working => ("Working", accent_style()),
        TeamActivity::WaitingForApproval => {
            ("Waiting for approval", status_style(StatusTone::Attention))
        }
        TeamActivity::WaitingForInput => ("Waiting for input", status_style(StatusTone::Attention)),
        TeamActivity::Error => ("Error", status_style(StatusTone::Failure)),
        TeamActivity::Unknown => ("Unknown", secondary_text_style()),
        TeamActivity::Unloaded => ("Unloaded", secondary_text_style()),
        TeamActivity::Archived => ("Archived", secondary_text_style()),
    };
    let (sync, sync_style) = match availability.synchronization {
        TeamSynchronization::Current => ("Current", status_style(StatusTone::Success)),
        TeamSynchronization::Disconnected => ("Disconnected", status_style(StatusTone::Attention)),
        TeamSynchronization::Unconfirmed => ("Unconfirmed", status_style(StatusTone::Attention)),
    };
    let mut readiness_line = field("Readiness", readiness, readiness_style);
    readiness_line.spans.extend([
        Span::styled(" · Activity  ", secondary_text_style()),
        Span::styled(activity, activity_style),
    ]);
    let mut lines = vec![readiness_line, field("Sync", sync, sync_style)];
    lines.extend(
        availability
            .reasons
            .iter()
            .map(|reason| Line::from(vec!["  ↳ ".into(), reason.clone().into()])),
    );
    lines
}

#[cfg(test)]
fn discovery_lines(discovery: &TeamAgentDiscovery) -> Vec<Line<'static>> {
    let availability = &discovery.availability;
    let mut lines = vec!["Team".bold().into()];
    lines.extend(availability_lines(availability));
    lines.extend([Line::default(), "Environment".bold().into()]);
    lines.extend(environment_lines(discovery));
    lines
}

fn environment_lines(discovery: &TeamAgentDiscovery) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for environment in &discovery.environments {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let selection = match (environment.current, environment.next) {
            (true, true) => "current + next",
            (true, false) => "current",
            (false, true) => "next",
            (false, false) => "last observed",
        };
        lines.push(Line::from(vec![
            environment.id.clone().into(),
            Span::styled(format!(" · {selection}"), secondary_text_style()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("OS  ", secondary_text_style()),
            environment
                .os
                .clone()
                .unwrap_or_else(|| "unknown".into())
                .into(),
            Span::styled(" · Shell  ", secondary_text_style()),
            environment
                .shell
                .clone()
                .unwrap_or_else(|| "unknown".into())
                .into(),
            Span::styled(" (cached)", secondary_text_style()),
        ]));
        if !environment.declared_capabilities.is_empty() {
            lines.push(field(
                "Declared",
                environment.declared_capabilities.join(", "),
                Style::default(),
            ));
        }
    }
    if discovery.environments.is_empty() {
        lines.push(Span::styled("Unknown", secondary_text_style()).into());
    }
    lines
}

fn execution_lines(row: &AgentsOverviewRow, agent: &codex_team::Agent) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let base_effort = row
        .thread
        .reasoning_effort
        .as_ref()
        .map(codex_protocol::openai_models::ReasoningEffort::as_str);
    for (label, model, effort) in [
        ("Current", &agent.current_model, &agent.current_effort),
        ("Next", &agent.next_model, &agent.next_effort),
    ] {
        // Unknown historical effort does not establish the execution model's default.
        if let Some(model) = model
            && (!row.metadata_known
                || row.thread.model.as_ref() != Some(model)
                || base_effort.is_none()
                || base_effort != effort.as_deref())
        {
            lines.push(field(
                label,
                format!("{model} / {}", effort.as_deref().unwrap_or("default")),
                Style::default(),
            ));
        }
    }
    if let Some(selection) = &agent.requested_selection {
        let applied = agent
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.applied_selection_revision)
            .is_some_and(|revision| revision >= selection.revision);
        let selected = |selection: &TeamSelection| {
            match selection {
                TeamSelection::Keep => "unchanged",
                TeamSelection::Default => "model default",
                TeamSelection::Inherit => "parent",
                TeamSelection::Value { value } => value.as_str(),
            }
            .to_owned()
        };
        lines.push(Line::from(vec![
            Span::styled(
                if applied { "Applied: " } else { "Queued: " },
                status_style(if applied {
                    StatusTone::Success
                } else {
                    StatusTone::Attention
                }),
            ),
            format!(
                "{} / {} · revision {}",
                selected(&selection.model),
                selected(&selection.effort),
                selection.revision
            )
            .into(),
        ]));
    }
    if let Some(runtime) = &agent.runtime {
        if runtime.in_flight > 0 {
            lines.push(field(
                "Admitted",
                format!("{} family operation(s)", runtime.in_flight),
                accent_style(),
            ));
        }
        if runtime.reconciling {
            lines.push(field(
                "Settling",
                "Previously admitted work",
                status_style(StatusTone::Attention),
            ));
        }
        if let Some(error) = &runtime.model_error {
            lines.push(field(
                "Error",
                error.clone(),
                status_style(StatusTone::Failure),
            ));
        }
        for job in &runtime.jobs {
            let (status, style) = match job.status {
                TeamJobStatus::Running => ("Running", accent_style()),
                TeamJobStatus::CancellationPending => {
                    ("Cancellation pending", status_style(StatusTone::Attention))
                }
                TeamJobStatus::Cancelled => ("Cancelled", status_style(StatusTone::Success)),
                TeamJobStatus::Unconfirmed => ("Unconfirmed", status_style(StatusTone::Attention)),
            };
            let mut line = Line::from(vec![
                Span::styled(format!("{}: ", job.tool), secondary_text_style()),
                Span::styled(status, style),
            ]);
            if let Some(detail) = &job.detail {
                line.spans.push(format!(" · {detail}").into());
            }
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
#[path = "agents_overview_team_tests.rs"]
mod tests;
