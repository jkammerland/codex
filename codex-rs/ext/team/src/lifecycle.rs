use std::sync::Weak;

use crate::delivery_evidence::AttemptOutcome;
use crate::delivery_evidence::runtime_attempt;
use anyhow::Result;
use codex_app_server_protocol::TeamExecutionMode;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnModelRequestInput;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TurnAbortReason;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::TeamHostService;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum HostEvent {
    Dispatch,
    Ready(String),
    SettingsChanged(String),
    Started {
        thread_id: String,
        turn_id: String,
    },
    Finished {
        thread_id: String,
        turn_id: String,
        interrupted: bool,
        runtime: String,
        error: Option<String>,
    },
}

impl TeamHostService {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn run(service: Weak<Self>, mut events: mpsc::UnboundedReceiver<HostEvent>) {
        // Terminal events survive handler failures and host restarts. Replaying
        // their receipt transitions never replays model or tool effects.
        if let Some(service) = service.upgrade()
            && let Some(store) = service.store.get()
        {
            match store
                .list::<HostEvent>("lifecycle_finished", "", u32::MAX)
                .await
            {
                Ok(pending) => {
                    for (_, event) in pending {
                        let _ = service.events.send(event);
                    }
                }
                Err(error) => tracing::error!(%error, "could not restore team lifecycle outcomes"),
            }
        }
        while let Some(event) = events.recv().await {
            let Some(service) = service.upgrade() else {
                break;
            };
            let _operation = service.operations.lock().await;
            let mut pending = Vec::new();
            if matches!(&event, HostEvent::Dispatch | HostEvent::Ready(_))
                && let Some(store) = service.store.get()
            {
                match store
                    .list::<HostEvent>("lifecycle_finished", "", u32::MAX)
                    .await
                {
                    Ok(saved) => pending.extend(saved.into_iter().map(|(_, event)| event)),
                    Err(error) => {
                        tracing::warn!(%error, "could not recover pending team lifecycle outcomes")
                    }
                }
            }
            pending.push(event);
            for event in pending {
                let finished = match &event {
                    HostEvent::Finished { runtime, .. } => Some(runtime.clone()),
                    _ => None,
                };
                if let Err(error) = service.handle_event(event).await {
                    tracing::warn!(%error, "team host event requires reconciliation");
                } else if let (Some(runtime), Some(store)) = (finished, service.store.get())
                    && let Err(error) = store.delete("lifecycle_finished", &runtime).await
                {
                    tracing::warn!(%error, "could not acknowledge team lifecycle outcome");
                }
            }
        }
    }

    async fn queue_finished(
        &self,
        thread: &codex_extension_api::ExtensionData,
        turn: &codex_extension_api::ExtensionData,
        interrupted: bool,
        error: Option<String>,
    ) {
        if ThreadId::from_string(thread.level_id())
            .ok()
            .and_then(|id| self.gate.root(id))
            .is_none()
        {
            return;
        }
        let runtime = runtime_attempt(turn);
        let saved = async {
            let event = HostEvent::Finished {
                thread_id: thread.level_id().to_string(),
                turn_id: turn.level_id().to_string(),
                interrupted,
                runtime: runtime.clone(),
                error,
            };
            if let Some(store) = self.store.get() {
                store.put("lifecycle_finished", &runtime, &event).await?;
            }
            let _ = self.events.send(event);
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = saved {
            tracing::error!(%error, "could not persist team lifecycle outcome");
        }
    }

    async fn handle_event(&self, event: HostEvent) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        let lifecycle_changed = !matches!(event, HostEvent::Dispatch);
        let previous_store_revision = store.revision();
        match event {
            HostEvent::Dispatch => {}
            HostEvent::Ready(thread_id) => self.reconcile_delivery(&thread_id).await?,
            HostEvent::SettingsChanged(thread_id) => {
                self.reconcile_model_selection(ThreadId::from_string(&thread_id)?)
                    .await?
            }
            HostEvent::Started { thread_id, turn_id } => {
                if self
                    .continuation_turn_id(ThreadId::from_string(&thread_id)?)
                    .await?
                    .is_some_and(|pending| pending != turn_id)
                {
                    store.delete("continuation", &thread_id).await?;
                    if let Some(mut enrollment) = store.enrollment(&thread_id).await? {
                        enrollment.continuation_turn_id = None;
                        store.update_enrollment(&enrollment).await?;
                    }
                }
                // An explicit user turn is the user's local resume action.
                if let Some(mut enrollment) = store.enrollment(&thread_id).await?
                    && enrollment.manually_held
                {
                    enrollment.manually_held = false;
                    store.update_enrollment(&enrollment).await?;
                }
                let thread_id = ThreadId::from_string(&thread_id)?;
                if self.continuation_turn_id(thread_id).await?.is_none() {
                    self.dispatch_delivery(
                        thread_id,
                        crate::delivery::DeliveryBoundary::TurnStart { turn_id: &turn_id },
                    )
                    .await?;
                }
            }
            HostEvent::Finished {
                thread_id,
                turn_id,
                interrupted,
                runtime,
                error,
            } => {
                if interrupted
                    && self
                        .gate
                        .is_manually_held(ThreadId::from_string(&thread_id)?)
                    && let Some(mut enrollment) = store.enrollment(&thread_id).await?
                {
                    enrollment.manually_held = true;
                    store.update_enrollment(&enrollment).await?;
                }
                let outcome = match error {
                    Some(error) => AttemptOutcome::Failed(error),
                    None if interrupted => AttemptOutcome::Interrupted,
                    None => AttemptOutcome::Completed,
                };
                self.finish_deliveries(&thread_id, &turn_id, &runtime, outcome)
                    .await?;
                self.reconcile_delivery(&thread_id).await?;
            }
        }
        self.cancel_jobs().await?;
        if store
            .control()
            .await?
            .is_some_and(|control| control.mode == TeamExecutionMode::Running)
            && let Some(manager) = self.manager.upgrade()
        {
            for thread_id in manager.list_thread_ids().await {
                let Some(root) = self.gate.root(thread_id) else {
                    continue;
                };
                let Some(enrollment) = store.enrollment(&root.to_string()).await? else {
                    continue;
                };
                if enrollment.manually_held || self.gate.try_enter(thread_id).is_none() {
                    continue;
                }
                let dispatched = async {
                    if !self.resume_continuation(thread_id).await? {
                        self.dispatch_delivery(
                            thread_id,
                            crate::delivery::DeliveryBoundary::IdleOrSafeBoundary,
                        )
                        .await?;
                        if let Ok(thread) = manager.get_thread(thread_id).await {
                            thread.resume_team_mailbox().await;
                        }
                    }
                    anyhow::Ok(())
                }
                .await;
                if let Err(error) = dispatched {
                    tracing::warn!(%thread_id, %error, "team member requires reconciliation");
                }
            }
        }
        // Publish after receipt transitions commit so an event-driven reader
        // can observe them. A no-op dispatch must not echo coordinator sync.
        if lifecycle_changed || store.revision() != previous_store_revision {
            self.notify();
        }
        Ok(())
    }
}

impl<C: Send + Sync + 'static> ThreadLifecycleContributor<C> for TeamHostService {
    fn on_model_selection_requested<'a>(
        &'a self,
        thread_id: ThreadId,
        settings: &'a codex_protocol::protocol::ThreadSettingsOverrides,
    ) -> ExtensionFuture<'a, Result<Option<i64>, String>> {
        Box::pin(async move {
            self.journal_model_selection(thread_id, settings)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn on_thread_settings_changed<'a>(
        &'a self,
        thread_store: &'a codex_extension_api::ExtensionData,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.events.send(HostEvent::SettingsChanged(
                thread_store.level_id().to_owned(),
            ));
        })
    }

    fn on_thread_ready<'a>(&'a self, input: ThreadReadyInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id())
                && let Err(error) = self.reconcile_model_selection(thread_id).await
            {
                tracing::warn!(%thread_id, %error, "team model settings require reconciliation");
            }
            let _ = self.control_events.send(());
            let _ = self
                .events
                .send(HostEvent::Ready(input.thread_store.level_id().to_string()));
        })
    }

    fn on_thread_idle<'a>(&'a self, _input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.events.send(HostEvent::Dispatch);
        })
    }
}

impl TurnLifecycleContributor for TeamHostService {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if ThreadId::from_string(input.thread_store.level_id())
                .ok()
                .and_then(|id| self.gate.root(id))
                .is_some()
            {
                if let Some(store) = self.store.get()
                    && let Err(error) = store
                        .delete("manual_hold", input.thread_store.level_id())
                        .await
                {
                    tracing::warn!(%error, "failed to persist local team resume");
                }
                let _ = self.events.send(HostEvent::Started {
                    thread_id: input.thread_store.level_id().to_string(),
                    turn_id: input.turn_store.level_id().to_string(),
                });
            }
        })
    }

    fn on_model_request<'a>(
        &'a self,
        input: TurnModelRequestInput<'a>,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.journal_model_request(input)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn on_model_acknowledged<'a>(
        &'a self,
        input: codex_extension_api::TurnModelAcknowledgedInput<'a>,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.journal_provider_acknowledgement(input)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if ThreadId::from_string(input.thread_store.level_id())
                .ok()
                .and_then(|id| self.gate.root(id))
                .is_none()
            {
                return;
            }
            if let Some(store) = self.store.get()
                && let Err(error) = store
                    .put(
                        "turn_error",
                        &runtime_attempt(input.turn_store),
                        &format!("{:?}", input.error),
                    )
                    .await
            {
                tracing::error!(%error, "could not persist team turn error");
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.queue_finished(
                input.thread_store,
                input.turn_store,
                false,
                input.terminal_error.map(|error| error.message.clone()),
            )
            .await;
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.reason == TurnAbortReason::Interrupted
                && let Some(store) = self.store.get()
                && ThreadId::from_string(input.thread_store.level_id())
                    .is_ok_and(|id| self.gate.is_manually_held(id))
                && let Err(error) = store
                    .put("manual_hold", input.thread_store.level_id(), &true)
                    .await
            {
                tracing::warn!(%error, "failed to persist local team hold");
            }
            self.queue_finished(input.thread_store, input.turn_store, true, None)
                .await;
        })
    }
}

#[cfg(test)]
#[path = "board_lifecycle_tests.rs"]
mod board_lifecycle_tests;
