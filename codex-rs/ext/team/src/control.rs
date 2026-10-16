use anyhow::Result;
use codex_app_server_protocol::TeamControlParams;
use codex_app_server_protocol::TeamControlResponse;
use codex_app_server_protocol::TeamExecutionMode;
use codex_core::RecoverTurnRequest;
use codex_core::StartIfIdleSubmission;
use codex_extension_api::ExecutionMode;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::CyberAccessProgram;
use serde::Deserialize;
use serde::Serialize;

use crate::TeamHostService;
use crate::lifecycle::HostEvent;

#[derive(Serialize, Deserialize)]
struct Continuation {
    turn_id: String,
    generation: String,
    history_offset: usize,
    cyber_access_program: Option<CyberAccessProgram>,
}

pub(crate) fn execution_mode(mode: TeamExecutionMode) -> ExecutionMode {
    match mode {
        TeamExecutionMode::Running => ExecutionMode::Running,
        TeamExecutionMode::Paused => ExecutionMode::Paused,
        TeamExecutionMode::Stopped => ExecutionMode::Stopped,
    }
}

impl TeamHostService {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub async fn control(&self, params: TeamControlParams) -> Result<TeamControlResponse> {
        let _update = self.control_updates.lock().await;
        let store = self.enable().await?;
        let control = store.apply_control(&params.control).await?;
        if control == params.control {
            self.confirmed_control_revision
                .fetch_max(control.revision, std::sync::atomic::Ordering::AcqRel);
        }
        self.apply_gate(&control).await?;
        let _ = self.control_events.send(());
        self.notify();
        let _ = self.events.send(HostEvent::Dispatch);
        Ok(TeamControlResponse {
            reconciled: control == params.control && self.pending_stops().await?.is_empty(),
            control,
        })
    }

    pub(crate) async fn stop_family(&self, revision: i64, roots: &[String]) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let Some(manager) = self.manager.upgrade() else {
            return Ok(());
        };
        let mut failures = Vec::new();
        for thread_id in manager.list_thread_ids().await {
            if !self
                .gate
                .root(thread_id)
                .is_some_and(|root| roots.contains(&root.to_string()))
            {
                continue;
            }
            let Ok(thread) = manager.get_thread(thread_id).await else {
                continue;
            };
            let saved = async {
                if let Some(turn_id) = thread.team_active_turn_id().await {
                thread.flush_rollout().await?;
                let history = thread.team_execution_history().await?;
                let mut continuation = Continuation {
                    turn_id: turn_id.clone(),
                    generation: history.generation,
                    history_offset: history.items.len(),
                    cyber_access_program: history
                        .items
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            RolloutItem::TurnContext(context) => {
                                Some(context.cyber_access_program)
                            }
                            _ => None,
                        })
                        .flatten(),
                };
                store
                    .put("continuation", &thread_id.to_string(), &continuation)
                    .await?;
                if let Some(mut enrollment) = store.enrollment(&thread_id.to_string()).await? {
                    enrollment.continuation_turn_id = Some(turn_id);
                    store.update_enrollment(&enrollment).await?;
                }
                thread.stop_team_turn().await?;
                thread.flush_rollout().await?;
                let stopped_history = thread.team_execution_history().await?;
                if stopped_history.items.iter().any(|item| matches!(item,
                    RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == continuation.turn_id)) {
                    // Completion can win the race with interruption. Resume
                    // must not restart work that already completed naturally.
                    store.delete("continuation", &thread_id.to_string()).await?;
                    if let Some(mut enrollment) = store.enrollment(&thread_id.to_string()).await? {
                        enrollment.continuation_turn_id = None;
                        store.update_enrollment(&enrollment).await?;
                    }
                } else {
                    continuation.history_offset = stopped_history.items.len();
                    store.put("continuation", &thread_id.to_string(), &continuation).await?;
                }
            }
                anyhow::Ok(())
            }.await;
            if let Err(error) = saved {
                // A history/storage failure must not let this or another
                // member keep executing after Stop. Recovery stays explicit.
                failures.push(format!("{thread_id}: {error}"));
                if let Err(error) = thread.stop_team_turn().await {
                    failures.push(format!("{thread_id}: {error}"));
                }
            }
            if thread.team_active_turn_id().await.is_none() {
                store
                    .put("stop_settled_revision", &thread_id.to_string(), &revision)
                    .await?;
                self.reconcile_delivery(&thread_id.to_string()).await?;
            }
            let terminals = async {
                for process in thread.list_background_terminals().await {
                    let confirmed = match process.process_id.parse() {
                        Ok(id) => thread.terminate_background_terminal(id).await,
                        Err(_) => false,
                    };
                    let id = format!("terminal:{thread_id}:{}", process.process_id);
                    store
                        .put(
                            "job",
                            &id,
                            &crate::jobs::StoredJob {
                                job: codex_app_server_protocol::TeamTrackedJob {
                                    id: id.clone(),
                                    thread_id: thread_id.to_string(),
                                    server: "local".into(),
                                    tool: "exec_command".into(),
                                    handle: Some(process.process_id),
                                    status: if confirmed {
                                        codex_app_server_protocol::TeamJobStatus::Cancelled
                                    } else {
                                        codex_app_server_protocol::TeamJobStatus::Unconfirmed
                                    },
                                    detail: (!confirmed).then(|| {
                                        "The terminal process did not confirm termination.".into()
                                    }),
                                },
                                adapter: None,
                                launch_revision: Some(revision),
                                requested_revision: None,
                                attempted_revision: None,
                                registration_fingerprint: None,
                            },
                        )
                        .await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = terminals {
                failures.push(format!("{thread_id}: {error}"));
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "Stop needs reconciliation: {}",
            failures.join("; ")
        );
        Ok(())
    }

    pub(crate) async fn resume_continuation(&self, thread_id: ThreadId) -> Result<bool> {
        let Some(store) = self.store.get() else {
            return Ok(false);
        };
        let Some(continuation) = store
            .get::<Continuation>("continuation", &thread_id.to_string())
            .await?
        else {
            return Ok(false);
        };
        let Some(manager) = self.manager.upgrade() else {
            return Ok(true);
        };
        let Ok(thread) = manager.get_thread(thread_id).await else {
            return Ok(true);
        };
        let history = thread.team_execution_history().await?;
        anyhow::ensure!(
            history.generation == continuation.generation,
            "stopped thread history was replaced; inspect it before resuming"
        );
        anyhow::ensure!(
            history.items.len() >= continuation.history_offset,
            "stopped turn history changed; inspect it before resuming"
        );
        let resumed = &history.items[continuation.history_offset..];
        if resumed.iter().any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == continuation.turn_id)) {
            store.delete("continuation", &thread_id.to_string()).await?;
            if let Some(mut enrollment) = store.enrollment(&thread_id.to_string()).await? {
                enrollment.continuation_turn_id = None;
                store.update_enrollment(&enrollment).await?;
            }
            return Ok(false);
        }
        if thread.team_active_turn_id().await.is_some() {
            return Ok(true);
        }
        if resumed.iter().any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == continuation.turn_id)) {
            // A continuation already reached its durable start boundary. Its
            // unfinished history belongs to normal recovery or human review;
            // repeated Resume commands must never replay it as a new attempt.
            return Ok(true);
        }
        if let StartIfIdleSubmission::Started { .. } = thread
            .recover_turn_if_idle(RecoverTurnRequest {
                turn_id: continuation.turn_id,
                thread_settings: Default::default(),
                trace: None,
                cyber_access_program: continuation.cyber_access_program,
            })
            .await?
        {
            thread.flush_rollout().await?;
        }
        Ok(true)
    }

    pub(crate) async fn continuation_turn_id(&self, thread_id: ThreadId) -> Result<Option<String>> {
        let Some(store) = self.store.get() else {
            return Ok(None);
        };
        Ok(store
            .get::<Continuation>("continuation", &thread_id.to_string())
            .await?
            .map(|continuation| continuation.turn_id))
    }
}
