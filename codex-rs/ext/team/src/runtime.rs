use std::collections::BTreeMap;

use anyhow::Result;
use codex_app_server_protocol::TeamAgentRuntime;
use codex_app_server_protocol::TeamEnrollment;
use codex_protocol::ThreadId;

use crate::TeamHostService;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

impl TeamHostService {
    pub(crate) async fn runtime(&self, roots: &[TeamEnrollment]) -> Result<Vec<TeamAgentRuntime>> {
        let Some(store) = self.store.get() else {
            return Ok(Vec::new());
        };
        let Some(manager) = self.manager.upgrade() else {
            return Ok(Vec::new());
        };
        let mut members = BTreeMap::new();
        // Enrollment remains visible even if its runtime was archived before
        // the coordinator first observed it.
        for root in roots {
            let id = root.address.thread_id.clone();
            members.insert(
                id.clone(),
                TeamAgentRuntime {
                    activity: Default::default(),
                    environments: Vec::new(),
                    thread_id: id.clone(),
                    root_thread_id: id,
                    parent_thread_id: None,
                    alias: root.alias.clone(),
                    loaded: false,
                    archived: false,
                    turn_id: None,
                    current_model: None,
                    current_effort: None,
                    next_model: None,
                    next_effort: None,
                    model_error: None,
                    model_origin: Default::default(),
                    effort_origin: Default::default(),
                    requested_selection_revision: None,
                    applied_selection_revision: None,
                    superseded_selection_revision: None,
                    in_flight: 0,
                    reconciling: true,
                    manually_held: root.manually_held,
                    continuation_turn_id: root.continuation_turn_id.clone(),
                    jobs: Vec::new(),
                },
            );
        }
        for (id, mut member) in store
            .list::<TeamAgentRuntime>("member", "", u32::MAX)
            .await?
        {
            if roots
                .iter()
                .any(|root| root.address.thread_id == member.root_thread_id)
            {
                member.loaded = false;
                member.turn_id = None;
                member.current_model = None;
                member.current_effort = None;
                member.in_flight = 0;
                member.activity = codex_app_server_protocol::TeamActivity::Unloaded;
                for environment in &mut member.environments {
                    environment.current = false;
                }
                members.insert(id, member);
            }
        }
        for thread_id in manager.list_thread_ids().await {
            let Some(root_id) = self.gate.root(thread_id) else {
                continue;
            };
            let Some(root) = roots
                .iter()
                .find(|root| root.address.thread_id == root_id.to_string())
            else {
                continue;
            };
            let Ok(thread) = manager.get_thread(thread_id).await else {
                continue;
            };
            let config = thread.config_snapshot().await;
            let active = thread.team_active_turn().await;
            let Some(gate) = self.gate.snapshot(thread_id) else {
                continue;
            };
            let alias = if thread_id == root_id {
                root.alias.clone()
            } else {
                format!(
                    "{}/{}",
                    root.alias,
                    config
                        .session_source
                        .get_nickname()
                        .unwrap_or_else(|| thread_id.to_string())
                )
            };
            let mut environments = BTreeMap::new();
            for selection in config.environment_selections() {
                environments.insert(
                    selection.environment_id.clone(),
                    codex_app_server_protocol::TeamExecutionEnvironment {
                        id: selection.environment_id.clone(),
                        next: true,
                        ..Default::default()
                    },
                );
            }
            for selection in thread
                .active_turn_environment_selections()
                .await
                .unwrap_or_default()
            {
                environments
                    .entry(selection.environment_id.clone())
                    .or_insert_with(|| codex_app_server_protocol::TeamExecutionEnvironment {
                        id: selection.environment_id,
                        ..Default::default()
                    })
                    .current = true;
            }
            for environment in environments.values_mut() {
                let Some(executor) = manager
                    .environment_manager()
                    .get_environment(&environment.id)
                else {
                    continue;
                };
                if !executor.is_remote() {
                    // Normal startup already resolved this thread's local shell. Local turns
                    // need not call environment/info, and thread overrides are not host-global.
                    environment.os = Some(std::env::consts::OS.to_owned());
                    environment.shell = Some(thread.team_local_shell_name().to_owned());
                } else if let Some(info) = executor.cached_info() {
                    environment.os = info.platform_os;
                    environment.shell = Some(info.shell.name);
                }
            }
            let member = TeamAgentRuntime {
                activity: match &active {
                    Some(turn) if turn.waiting_for_approval => {
                        codex_app_server_protocol::TeamActivity::WaitingForApproval
                    }
                    Some(turn) if turn.waiting_for_input => {
                        codex_app_server_protocol::TeamActivity::WaitingForInput
                    }
                    Some(_) => codex_app_server_protocol::TeamActivity::Working,
                    None => codex_app_server_protocol::TeamActivity::Idle,
                },
                environments: environments.into_values().collect(),
                thread_id: thread_id.to_string(),
                root_thread_id: root_id.to_string(),
                parent_thread_id: config.parent_thread_id.map(|id| id.to_string()),
                alias,
                loaded: true,
                archived: false,
                turn_id: active.as_ref().map(|turn| turn.turn_id.clone()),
                current_model: active.as_ref().map(|turn| turn.model.clone()),
                current_effort: active.and_then(|turn| turn.effort),
                next_model: Some(config.model),
                next_effort: config.reasoning_effort.map(|effort| effort.to_string()),
                model_error: store
                    .get::<String>("model_recovery_error", &thread_id.to_string())
                    .await?
                    .or(thread.team_model_error().await),
                model_origin: config.team_model_policy.model_origin,
                effort_origin: config.team_model_policy.effort_origin,
                requested_selection_revision: store
                    .get::<serde_json::Value>("model_selection_request", &thread_id.to_string())
                    .await?
                    .and_then(|request| {
                        request
                            .pointer("/selection/revision")
                            .and_then(serde_json::Value::as_i64)
                    }),
                applied_selection_revision: config
                    .team_model_policy
                    .selection
                    .as_ref()
                    .map(|selection| selection.revision),
                superseded_selection_revision: config.team_model_policy.superseded_revision.max(
                    store
                        .get::<i64>("model_selection_superseded", &thread_id.to_string())
                        .await?,
                ),
                in_flight: gate.in_flight.try_into().unwrap_or(u32::MAX),
                reconciling: gate.reconciling,
                manually_held: self.gate.is_manually_held(thread_id),
                continuation_turn_id: self.continuation_turn_id(thread_id).await?,
                jobs: self.jobs(&thread_id.to_string()).await?,
            };
            store.put("member", &thread_id.to_string(), &member).await?;
            if thread_id != root_id
                && store
                    .get::<String>("member_credential", &thread_id.to_string())
                    .await?
                    .is_none()
            {
                store
                    .put(
                        "member_credential",
                        &thread_id.to_string(),
                        &uuid::Uuid::new_v4().to_string(),
                    )
                    .await?;
            }
            members.insert(thread_id.to_string(), member);
        }
        for member in members.values_mut() {
            let thread_id = ThreadId::from_string(&member.thread_id)?;
            if !member.loaded {
                member.archived = manager.is_thread_archived(thread_id).await?;
            }
            member.manually_held = self.gate.is_manually_held(thread_id);
            member.reconciling = self
                .gate
                .snapshot(thread_id)
                .is_some_and(|gate| gate.reconciling);
            member.model_error = store
                .get::<String>("model_recovery_error", &member.thread_id)
                .await?
                .or(member.model_error.take());
            member.superseded_selection_revision = member.superseded_selection_revision.max(
                store
                    .get::<i64>("model_selection_superseded", &member.thread_id)
                    .await?,
            );
            member.jobs = self.jobs(&member.thread_id).await?;
        }
        Ok(members.into_values().collect())
    }
}
