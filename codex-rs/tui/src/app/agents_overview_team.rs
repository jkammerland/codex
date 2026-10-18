//! Enrollment-backed task rows keep unloaded Board-only teammates discoverable.
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;

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
                lines: vec!["History metadata: unknown (enrollment only)".into()],
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

pub(super) fn discovery_lines(discovery: &TeamAgentDiscovery) -> Vec<ratatui::text::Line<'static>> {
    let availability = &discovery.availability;
    let mut lines = vec![
        format!("Synchronization: {:?}", availability.synchronization).into(),
        format!(
            "Activity: {:?} · Readiness: {:?}",
            availability.activity, availability.readiness
        )
        .into(),
    ];
    lines.extend(availability.reasons.iter().cloned().map(Into::into));
    for environment in &discovery.environments {
        let selection = match (environment.current, environment.next) {
            (true, true) => "current + next",
            (true, false) => "current",
            (false, true) => "next",
            (false, false) => "last observed",
        };
        lines.push(
            format!(
                "Environment: {} ({selection}) · OS {} · shell {} (cached)",
                environment.id,
                environment.os.as_deref().unwrap_or("unknown"),
                environment.shell.as_deref().unwrap_or("unknown")
            )
            .into(),
        );
        if !environment.declared_capabilities.is_empty() {
            lines
                .push(format!("Declared: {}", environment.declared_capabilities.join(", ")).into());
        }
    }
    if discovery.environments.is_empty() {
        lines.push("Environments: unknown".into());
    }
    lines
}

#[cfg(test)]
#[path = "agents_overview_team_tests.rs"]
mod tests;
