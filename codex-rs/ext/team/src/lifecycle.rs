use std::sync::Weak;

use anyhow::Result;
use codex_app_server_protocol::TeamExecutionMode;
use codex_app_server_protocol::TeamReceiptState;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadReadyInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TurnAbortReason;
use tokio::sync::mpsc;

use crate::TeamHostService;

pub(crate) enum HostEvent {
    Dispatch,
    Ready(String),
    Started {
        thread_id: String,
        turn_id: String,
    },
    Finished {
        thread_id: String,
        turn_id: String,
        interrupted: bool,
    },
}

impl TeamHostService {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn run(service: Weak<Self>, mut events: mpsc::UnboundedReceiver<HostEvent>) {
        while let Some(event) = events.recv().await {
            let Some(service) = service.upgrade() else {
                break;
            };
            let _operation = service.operations.lock().await;
            if let Err(error) = service.handle_event(event).await {
                tracing::warn!(%error, "team host event requires reconciliation");
            }
        }
    }

    async fn handle_event(&self, event: HostEvent) -> Result<()> {
        let Some(store) = self.store.get() else {
            return Ok(());
        };
        match event {
            HostEvent::Dispatch => {}
            HostEvent::Ready(thread_id) => self.reconcile_delivery(&thread_id).await?,
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
                for (_, mut receipt) in store.deliveries(&thread_id).await? {
                    if receipt.turn_id.as_deref() == Some(&turn_id)
                        && matches!(
                            receipt.state,
                            TeamReceiptState::Accepted | TeamReceiptState::Interrupted
                        )
                    {
                        receipt.state = if interrupted {
                            TeamReceiptState::Interrupted
                        } else {
                            TeamReceiptState::Completed
                        };
                        store.update_receipt(&receipt).await?;
                    }
                }
            }
        }
        self.cancel_jobs().await?;
        self.notify();
        if store
            .control()
            .await?
            .is_some_and(|control| control.mode == TeamExecutionMode::Running)
        {
            let Some(manager) = self.manager.upgrade() else {
                return Ok(());
            };
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
        Ok(())
    }
}

impl<C: Send + Sync + 'static> ThreadLifecycleContributor<C> for TeamHostService {
    fn on_thread_settings_changed<'a>(
        &'a self,
        _thread_store: &'a codex_extension_api::ExtensionData,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.events.send(HostEvent::Dispatch);
        })
    }

    fn on_thread_ready<'a>(&'a self, input: ThreadReadyInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
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

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.events.send(HostEvent::Finished {
                thread_id: input.thread_store.level_id().to_string(),
                turn_id: input.turn_store.level_id().to_string(),
                interrupted: false,
            });
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.reason == TurnAbortReason::Interrupted {
                if let Some(store) = self.store.get()
                    && ThreadId::from_string(input.thread_store.level_id())
                        .is_ok_and(|id| self.gate.is_manually_held(id))
                    && let Err(error) = store
                        .put("manual_hold", input.thread_store.level_id(), &true)
                        .await
                {
                    tracing::warn!(%error, "failed to persist local team hold");
                }
                let _ = self.events.send(HostEvent::Finished {
                    thread_id: input.thread_store.level_id().to_string(),
                    turn_id: input.turn_store.level_id().to_string(),
                    interrupted: true,
                });
            }
        })
    }
}
