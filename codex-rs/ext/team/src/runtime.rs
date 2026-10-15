use std::collections::BTreeMap;

use anyhow::Result;
use codex_app_server_protocol::TeamAgentRuntime;
use codex_app_server_protocol::TeamEnrollment;
use codex_protocol::ThreadId;

use crate::TeamHostService;

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
            let active = thread.team_active_turn_model().await;
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
            let member = TeamAgentRuntime {
                thread_id: thread_id.to_string(),
                root_thread_id: root_id.to_string(),
                parent_thread_id: thread
                    .session_configured()
                    .parent_thread_id
                    .map(|id| id.to_string()),
                alias,
                loaded: true,
                archived: false,
                turn_id: active.as_ref().map(|(id, _, _)| id.clone()),
                current_model: active.as_ref().map(|(_, model, _)| model.clone()),
                current_effort: active.and_then(|(_, _, effort)| effort),
                next_model: Some(config.model),
                next_effort: config.reasoning_effort.map(|effort| effort.to_string()),
                model_error: thread.team_model_error().await,
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
            member.jobs = self.jobs(&member.thread_id).await?;
        }
        Ok(members.into_values().collect())
    }
}
