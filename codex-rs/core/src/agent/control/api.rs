//! Implements shared controller operations using the existing local runtime helpers.
//! Runtime loading, message delivery and shared state remain in their existing modules.

use super::LocalAgentControl;
use super::spawn::SpawnInitialInput;
use crate::agent::api::AgentConfigUpdate;
use crate::agent::api::AgentControl;
use crate::agent::api::AgentInfo;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::AgentTurnOutcome;
use crate::agent::api::DeliveryReceipt;
use crate::agent::api::SendRequest;
use crate::agent::api::SpawnRequest;
use crate::agent::types::AgentExecutionGuard;
use crate::agent::types::LiveAgent;
use crate::agent::types::MessageDeliveryMode;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::GuardianRootSnapshot;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::rollout_budget::RolloutBudgetReminder;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::ThreadTraceContext;
use futures::future::BoxFuture;
use std::collections::HashSet;

impl AgentControl for LocalAgentControl {
    fn identity(&self) -> SessionId {
        self.session_id()
    }

    fn supports_task_barriers(&self) -> bool {
        true
    }

    fn watch_agent_work(&self) -> tokio::sync::watch::Receiver<u64> {
        self.runtime.work_updates.subscribe()
    }

    fn set_task_barrier(&self, agent: ThreadId, task_ids: Option<Vec<String>>) {
        let mut barriers = self
            .runtime
            .barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(task_ids) = task_ids {
            barriers.insert(agent, task_ids);
        } else {
            barriers.remove(&agent);
        }
        self.runtime
            .work_updates
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    fn tasks(
        &self,
        filter: codex_thread_store::AgentTaskFilter,
    ) -> BoxFuture<'_, Result<codex_thread_store::AgentTaskSnapshot>> {
        Box::pin(async move {
            self.mail_store()
                .await?
                .tasks(self.session_id.into(), filter)
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))
        })
    }

    fn agent_work(
        &self,
        agent: ThreadId,
    ) -> BoxFuture<'_, Result<Option<crate::agent::api::AgentWorkSummary>>> {
        Box::pin(async move { self.work_summary(agent).await.map(Some) })
    }

    fn restore_attached_agents(&self, root: ThreadId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if root != ThreadId::from(self.session_id) {
                return Ok(());
            }
            let manager = self.runtime.upgrade()?;
            let root_thread = manager.get_thread(root).await?;
            if !root_thread
                .session
                .enabled(codex_features::Feature::WakeOnAgentMessages)
            {
                return Ok(());
            }
            let mut config = root_thread.config().await.as_ref().clone();
            config.base_instructions = None;
            config.base_instructions_provenance = None;
            config.restore_saved_model_policy = true;
            for metadata in self.runtime.registry.live_agents() {
                let Some(agent) = metadata.agent_id.filter(|id| *id != root) else {
                    continue;
                };
                if manager.get_thread(agent).await.is_ok() {
                    continue;
                }
                let pending = self.pending_mail(agent).await?;
                if pending.iter().any(|mail| {
                    mail.mail.as_ref().is_some_and(|metadata| {
                        metadata.wake == codex_protocol::protocol::AgentMailWake::Auto
                    })
                }) && let Err(err) = self
                    .ensure_v2_agent_loaded(config.clone(), agent, /*parent*/ None)
                    .await
                {
                    tracing::warn!("pending recipient could not be restored: {err}");
                    self.mail_blocked(agent, codex_thread_store::AgentTaskStatus::Unavailable)
                        .await?;
                }
            }
            Ok(())
        })
    }

    fn mail_blocked(
        &self,
        agent: ThreadId,
        status: codex_thread_store::AgentTaskStatus,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let store = self.mail_store().await?;
            let root = self.session_id.into();
            self.mutate_work(async move {
                let snapshot = store
                    .tasks(
                        root,
                        codex_thread_store::AgentTaskFilter {
                            target_id: Some(agent),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                for mut task in snapshot
                    .tasks
                    .into_iter()
                    .filter(|task| !task.status.is_terminal())
                {
                    task.status = status;
                    store
                        .put_task(task)
                        .await
                        .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                }
                Ok(())
            })
            .await
        })
    }

    fn pending_mail(
        &self,
        agent: ThreadId,
    ) -> BoxFuture<'_, Result<Vec<codex_protocol::protocol::InterAgentCommunication>>> {
        Box::pin(async move {
            let store = self.mail_store().await?;
            let terminal = store
                .tasks(
                    self.session_id.into(),
                    codex_thread_store::AgentTaskFilter {
                        target_id: Some(agent),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?
                .tasks
                .into_iter()
                .filter(|task| task.status.is_terminal())
                .map(|task| task.task_id)
                .collect::<HashSet<_>>();
            let records = store
                .pending(self.session_id.into(), agent)
                .await
                .map_err(|err| CodexErr::Fatal(err.to_string()))?;
            records
                .into_iter()
                .map(|record| {
                    serde_json::from_str::<codex_protocol::protocol::InterAgentCommunication>(
                        &record.envelope.payload_json,
                    )
                    .map_err(|err| CodexErr::Fatal(err.to_string()))
                })
                .filter(|result| {
                    !result.as_ref().is_ok_and(|mail| {
                        mail.mail.as_ref().is_some_and(|metadata| {
                            metadata.kind == codex_protocol::protocol::AgentMailKind::Assignment
                                && metadata
                                    .task_id
                                    .as_ref()
                                    .is_some_and(|id| terminal.contains(id))
                        })
                    })
                })
                .collect()
        })
    }

    fn mail_recorded<'a>(
        &'a self,
        agent: ThreadId,
        turn: &'a str,
        message_id: String,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let store = self.mail_store().await?;
            let root = self.session_id.into();
            let turn = turn.to_owned();
            self.mutate_work(async move {
                store
                    .mark_recorded(root, agent, vec![message_id], turn)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))
            })
            .await
        })
    }

    fn mail_attempted<'a>(
        &'a self,
        agent: ThreadId,
        turn: &'a str,
        items: &'a [codex_protocol::models::ResponseItem],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let ids = items
                .iter()
                .filter_map(|item| match item {
                    codex_protocol::models::ResponseItem::AgentMessage { id, .. }
                    | codex_protocol::models::ResponseItem::Message { id, .. } => {
                        id.as_ref().map(ToString::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Ok(());
            }
            let store = self.mail_store().await?;
            let root = self.session_id.into();
            let turn = turn.to_owned();
            self.mutate_work(async move {
                let ids = store
                    .accepted_ids(root, agent, ids)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                if ids.is_empty() {
                    return Ok(());
                }
                store
                    .mark_attempted(root, agent, ids, turn)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                Ok(())
            })
            .await
        })
    }

    fn mail_submitted<'a>(
        &'a self,
        agent: ThreadId,
        turn: &'a str,
        items: &'a [codex_protocol::models::ResponseItem],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let ids = items
                .iter()
                .filter_map(|item| match item {
                    codex_protocol::models::ResponseItem::AgentMessage { id, .. }
                    | codex_protocol::models::ResponseItem::Message { id, .. } => {
                        id.as_ref().map(ToString::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if ids.is_empty() {
                return Ok(());
            }
            let store = self.mail_store().await?;
            let root = self.session_id.into();
            let turn = turn.to_owned();
            self.mutate_work(async move {
                let ids = store
                    .accepted_ids(root, agent, ids)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                if ids.is_empty() {
                    return Ok(());
                }
                store
                    .mark_submitted(root, agent, ids, turn)
                    .await
                    .map_err(|err| CodexErr::Fatal(err.to_string()))?;
                Ok(())
            })
            .await
        })
    }

    fn resolve<'a>(
        &'a self,
        caller: ThreadId,
        parent: Option<ThreadId>,
        source: &'a SessionSource,
        target: &'a str,
    ) -> BoxFuture<'a, Result<ThreadId>> {
        Box::pin(async move {
            self.runtime.register_session_root(caller, parent);
            if let Ok(thread_id) = ThreadId::from_string(target) {
                return Ok(thread_id);
            }
            self.runtime
                .resolve_agent_reference(caller, source, target)
                .await
        })
    }

    fn spawn(
        &self,
        request: SpawnRequest,
    ) -> BoxFuture<'_, Result<(LiveAgent, ThreadConfigSnapshot)>> {
        Box::pin(async move {
            let SpawnRequest {
                caller,
                config,
                input,
                source,
                options,
            } = request;
            let input = match input {
                AgentInput::UserInput(input) => SpawnInitialInput::UserInput(input),
                AgentInput::Message { message, mode } => {
                    if mode != MessageDeliveryMode::TriggerTurn {
                        return Err(CodexErr::InvalidRequest(
                            "spawn input must start the child turn".to_string(),
                        ));
                    }
                    let recipient = source.get_agent_path().ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "spawned agent is missing a canonical task name".to_string(),
                        )
                    })?;
                    let author = recipient
                        .as_str()
                        .rsplit_once('/')
                        .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
                        .ok_or_else(|| {
                            CodexErr::InvalidRequest("spawn input needs a child path".to_string())
                        })?;
                    SpawnInitialInput::InterAgentCommunication(
                        message.into_communication(author, recipient, mode),
                        AgentCommunicationContext::new(AgentCommunicationKind::Spawn, caller),
                    )
                }
            };
            Box::pin(self.spawn_agent_internal(config, input, Some(source), options)).await
        })
    }

    fn send(&self, request: SendRequest) -> BoxFuture<'_, Result<DeliveryReceipt>> {
        Box::pin(async move {
            let SendRequest {
                caller,
                target,
                resume_config,
                input,
                mut start_options,
            } = request;
            let target = self.resolve_target(caller, &target)?;
            let track_assignment = resume_config
                .features
                .enabled(codex_features::Feature::WakeOnAgentMessages);
            let (metadata, submission_id, task_id) = match input {
                AgentInput::UserInput(input) => {
                    let receiver = self.get_agent_metadata(target);
                    let _residency_pin = if receiver.is_some() {
                        self.ensure_v2_agent_loaded(resume_config, target, /*parent*/ None)
                            .await?
                    } else {
                        None
                    };
                    let submission_id = self.send_input(target, input, start_options).await?;
                    (receiver.unwrap_or_default(), submission_id, None)
                }
                AgentInput::Message { message, mode } => {
                    let receiver = self.runtime.ensure_agent_known(target)?;
                    let author = self
                        .runtime
                        .ensure_agent_known(caller)?
                        .agent_path
                        .unwrap_or_else(AgentPath::root);
                    if mode == MessageDeliveryMode::TriggerTurn
                        && receiver.agent_path.as_ref().is_some_and(AgentPath::is_root)
                    {
                        return Err(CodexErr::UnsupportedOperation(
                            "Follow-up tasks can't target the root agent".to_string(),
                        ));
                    }
                    let receiver_path = receiver.agent_path.clone().ok_or_else(|| {
                        CodexErr::UnsupportedOperation(
                            "target agent is missing an agent_path".to_string(),
                        )
                    })?;
                    // Cold-restored children still reload lazily on any message. Only
                    // locally evicted recipients can retain mail without reloading.
                    // Loaded recipients go straight to delivery, which rejects sends
                    // racing an in-progress eviction when it acquires the residency pin.
                    let _residency_pin = if mode == MessageDeliveryMode::TriggerTurn
                        || (mode == MessageDeliveryMode::Auto
                            && resume_config
                                .features
                                .enabled(codex_features::Feature::WakeOnAgentMessages))
                        || (self.runtime.upgrade()?.get_thread(target).await.is_err()
                            && self.runtime.registry.evicted_environments(target).is_none())
                    {
                        self.ensure_v2_agent_loaded(resume_config, target, /*parent*/ None)
                            .await?
                    } else {
                        None
                    };
                    let communication = message.into_communication(author, receiver_path, mode);
                    let task_id = communication
                        .mail
                        .as_ref()
                        .and_then(|mail| mail.task_id.clone())
                        .filter(|_| track_assignment);
                    let kind = match mode {
                        MessageDeliveryMode::Auto | MessageDeliveryMode::QueueOnly => {
                            start_options.parent_turn_id = None;
                            AgentCommunicationKind::Message
                        }
                        MessageDeliveryMode::TriggerTurn => AgentCommunicationKind::Followup,
                    };
                    let submission_id = self
                        .send_inter_agent_communication(
                            target,
                            communication,
                            AgentCommunicationContext::new(kind, caller),
                            start_options,
                        )
                        .await?;
                    (receiver, submission_id, task_id)
                }
            };
            Ok(DeliveryReceipt {
                task_id,
                thread_id: target,
                metadata,
                submission_id,
            })
        })
    }

    fn take_mailbox(&self, agent: ThreadId) -> Vec<crate::PendingAgentMessage> {
        self.runtime.mailboxes.take(agent)
    }

    fn watch_mailbox(&self, agent: ThreadId) -> tokio::sync::watch::Receiver<bool> {
        self.runtime.mailboxes.watch(agent)
    }

    fn ensure_child_loaded(&self, parent: ThreadId, child: ThreadId) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let parent = self.runtime.upgrade()?.get_thread(parent).await?;
            let config = parent.session.get_config().await.as_ref().clone();
            self.ensure_v2_agent_loaded(config, child, Some(parent))
                .await
                .map(drop)
        })
    }

    fn interrupt(
        &self,
        caller: ThreadId,
        target: AgentTarget,
        version: MultiAgentVersion,
    ) -> BoxFuture<'_, Result<AgentInfo>> {
        Box::pin(async move {
            let target = self.resolve_target(caller, &target)?;
            match version {
                MultiAgentVersion::Disabled | MultiAgentVersion::V1 => {
                    let snapshot = self.inspect_agent(target).await?;
                    self.interrupt_agent(target).await?;
                    Ok(snapshot)
                }
                MultiAgentVersion::V2 => self.interrupt_spawned_agent(caller, target).await,
            }
        })
    }

    fn list<'a>(
        &'a self,
        caller: ThreadId,
        parent: Option<ThreadId>,
        source: &'a SessionSource,
        path_prefix: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<LiveAgent>>> {
        Box::pin(async move {
            self.runtime.register_session_root(caller, parent);
            self.list_agents(source, path_prefix).await
        })
    }

    fn child_agent_paths(&self, parent: ThreadId) -> BoxFuture<'_, Vec<AgentPath>> {
        Box::pin(async move {
            let Some(parent_path) = self
                .runtime
                .registry
                .agent_metadata_for_thread(parent)
                .and_then(|metadata| metadata.agent_path)
            else {
                return Vec::new();
            };
            let parent_prefix = format!("{parent_path}/");
            let mut agent_paths = self
                .runtime
                .registry
                .live_agents()
                .into_iter()
                .filter_map(|metadata| metadata.agent_path)
                .filter(|path| {
                    path.as_str()
                        .strip_prefix(&parent_prefix)
                        .is_some_and(|name| !name.contains('/'))
                })
                .collect::<Vec<_>>();
            let loaded_paths = self
                .runtime
                .open_thread_spawn_children(parent)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(_, metadata)| metadata.agent_path)
                .collect::<HashSet<_>>();
            agent_paths.sort();
            // Stable sorting preserves alphabetical order within each group.
            agent_paths.sort_by_key(|path| !loaded_paths.contains(path));
            agent_paths
        })
    }

    fn check_turn_admission(
        &self,
        version: MultiAgentVersion,
        source: &SessionSource,
    ) -> Result<()> {
        self.ensure_execution_capacity(version, source)
    }

    fn admit_turn(
        &self,
        version: MultiAgentVersion,
        source: &SessionSource,
    ) -> Option<AgentExecutionGuard> {
        self.execution_guard(version, source)
    }

    fn record_usage(&self, usage: TokenUsage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.record_rollout_budget_usage(&usage) })
    }

    fn turn_finished<'a>(
        &'a self,
        outcome: AgentTurnOutcome,
        trace: &'a ThreadTraceContext,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.notify_parent_of_terminal_turn(outcome, trace))
    }

    fn service_tier(&self) -> Option<String> {
        self.root_service_tier()
    }

    fn propagate_config_update(&self, update: AgentConfigUpdate) {
        match update {
            AgentConfigUpdate::ServiceTier(tier) => self.set_root_service_tier(tier),
        }
    }

    fn get_guardian_package(&self, agent: ThreadId) -> BoxFuture<'_, Option<GuardianRootSnapshot>> {
        Box::pin(self.root_user_authorization(agent))
    }

    fn pending_budget_reminder<'a>(
        &'a self,
        agent: ThreadId,
        window: &'a str,
    ) -> BoxFuture<'a, Option<RolloutBudgetReminder>> {
        Box::pin(async move { LocalAgentControl::pending_budget_reminder(self, agent, window) })
    }

    fn mark_budget_reminder_delivered<'a>(
        &'a self,
        agent: ThreadId,
        window: &'a str,
        reminder: RolloutBudgetReminder,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            LocalAgentControl::mark_budget_reminder_delivered(self, agent, window, reminder);
        })
    }
}
