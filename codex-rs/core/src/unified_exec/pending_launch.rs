use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default)]
pub(super) struct PendingLaunchOutcome {
    pub(super) termination_error: Option<String>,
    pub(super) network_approval_error: Option<String>,
}

#[derive(Debug)]
pub(super) enum PendingLaunchDecision {
    Cleanup {
        terminal_event: PendingLaunchTerminalEvent,
    },
    WatchProcess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingLaunchTerminalEvent {
    Suppress,
    Emit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingLaunchEventVisibility {
    Pending,
    Emit,
    Suppress,
}

#[derive(Debug)]
enum PendingLaunchState {
    Reserved,
    Launching {
        cleanup_requested: bool,
    },
    Published {
        decision_tx: oneshot::Sender<PendingLaunchDecision>,
    },
    EmittingBegin {
        decision_tx: oneshot::Sender<PendingLaunchDecision>,
        cleanup_requested: bool,
        origin_abandoned: bool,
    },
    Begun {
        decision_tx: oneshot::Sender<PendingLaunchDecision>,
    },
    Cleaning,
    WatchingProcess,
    Complete,
}

#[derive(Debug)]
pub(super) struct PendingLaunch {
    state: Mutex<PendingLaunchState>,
    completed: CancellationToken,
    outcome: Mutex<PendingLaunchOutcome>,
    event_visibility: watch::Sender<PendingLaunchEventVisibility>,
    manager_tasks: Mutex<ManagerTasks>,
}

#[derive(Clone, Debug)]
struct ManagerTaskControl {
    cancellation: CancellationToken,
    completed: CancellationToken,
}

#[derive(Clone, Debug)]
pub(super) struct ManagerTaskHandle {
    control: ManagerTaskControl,
}

impl ManagerTaskHandle {
    pub(super) async fn wait(&self) {
        self.control.completed.cancelled().await;
    }

    pub(super) async fn cancel_and_wait(&self) {
        self.control.cancellation.cancel();
        self.wait().await;
    }
}

#[derive(Debug, Default)]
struct ManagerTasks {
    force_cancelled: bool,
    active: Vec<ManagerTaskControl>,
}

impl PendingLaunch {
    pub(super) fn new() -> Arc<Self> {
        let (event_visibility, _) = watch::channel(PendingLaunchEventVisibility::Pending);
        Arc::new(Self {
            state: Mutex::new(PendingLaunchState::Reserved),
            completed: CancellationToken::new(),
            outcome: Mutex::new(PendingLaunchOutcome::default()),
            event_visibility,
            manager_tasks: Mutex::new(ManagerTasks::default()),
        })
    }

    fn register_manager_task(&self) -> ManagerTaskControl {
        let control = ManagerTaskControl {
            cancellation: CancellationToken::new(),
            completed: CancellationToken::new(),
        };
        let mut manager_tasks = self
            .manager_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        manager_tasks
            .active
            .retain(|task| !task.completed.is_cancelled());
        if manager_tasks.force_cancelled {
            control.cancellation.cancel();
        }
        manager_tasks.active.push(control.clone());
        control
    }

    pub(super) fn spawn_manager_task<Fut>(self: &Arc<Self>, task: Fut) -> ManagerTaskHandle
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let task_control = self.register_manager_task();
        let task_owner = Arc::clone(self);
        let cancellation = task_control.cancellation.clone();
        let completed = task_control.completed.clone();
        let task = tokio::spawn(async move {
            let mut task = Box::pin(task);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {}
                _ = &mut task => {}
            }
            drop(task);
            drop(task_owner);
        });
        tokio::spawn(async move {
            // JoinHandle completion is the point at which Tokio has torn down
            // the managed future, including every resource captured by it.
            let _ = task.await;
            completed.cancel();
        });
        ManagerTaskHandle {
            control: task_control,
        }
    }

    /// Force-cancel manager-owned work after the session-shutdown grace. Live
    /// cleanup intentionally does not use this escape hatch.
    pub(super) async fn force_shutdown(&self, outcome: PendingLaunchOutcome) {
        if *self.event_visibility.borrow() == PendingLaunchEventVisibility::Pending {
            self.suppress_events();
        }
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outcome;
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PendingLaunchState::Complete;

        self.cancel_manager_tasks_and_wait().await;
        self.completed.cancel();
    }

    /// Cancels all manager work and waits for captured resources to be dropped.
    async fn cancel_manager_tasks_and_wait(&self) {
        {
            let mut manager_tasks = self
                .manager_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manager_tasks.force_cancelled = true;
            for task in &manager_tasks.active {
                task.cancellation.cancel();
            }
        }

        // A task that was already running when force-cancellation began can
        // register child work before it observes its cancellation. Such work
        // is born cancelled, but still needs to be polled and dropped before
        // shutdown may claim that manager-owned resources have been severed.
        loop {
            let tasks = {
                let mut manager_tasks = self
                    .manager_tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                manager_tasks
                    .active
                    .retain(|task| !task.completed.is_cancelled());
                for task in &manager_tasks.active {
                    task.cancellation.cancel();
                }
                manager_tasks.active.clone()
            };
            if tasks.is_empty() {
                break;
            }
            futures::future::join_all(tasks.iter().map(|task| task.completed.cancelled())).await;
        }
    }

    pub(super) fn subscribe_event_visibility(
        &self,
    ) -> watch::Receiver<PendingLaunchEventVisibility> {
        self.event_visibility.subscribe()
    }

    #[cfg(test)]
    pub(super) fn emit_events_for_test(&self) {
        self.event_visibility
            .send_replace(PendingLaunchEventVisibility::Emit);
    }

    pub(super) fn begin(self: &Arc<Self>) -> Result<PendingLaunchGuard, PendingLaunchError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            PendingLaunchState::Reserved => {
                *state = PendingLaunchState::Launching {
                    cleanup_requested: false,
                };
                Ok(PendingLaunchGuard {
                    launch: Arc::clone(self),
                    handed_off: false,
                })
            }
            PendingLaunchState::Launching { .. }
            | PendingLaunchState::Published { .. }
            | PendingLaunchState::EmittingBegin { .. }
            | PendingLaunchState::Begun { .. }
            | PendingLaunchState::Cleaning
            | PendingLaunchState::WatchingProcess
            | PendingLaunchState::Complete => Err(PendingLaunchError),
        }
    }

    pub(super) fn publish(
        &self,
    ) -> Result<oneshot::Receiver<PendingLaunchDecision>, PendingLaunchError> {
        let (decision_tx, decision_rx) = oneshot::channel();
        let mut decision_tx = Some(decision_tx);
        let decision = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*state {
                PendingLaunchState::Launching { cleanup_requested } => {
                    if *cleanup_requested {
                        *state = PendingLaunchState::Cleaning;
                        Some(PendingLaunchDecision::Cleanup {
                            terminal_event: PendingLaunchTerminalEvent::Suppress,
                        })
                    } else {
                        let Some(decision_tx) = decision_tx.take() else {
                            return Err(PendingLaunchError);
                        };
                        *state = PendingLaunchState::Published { decision_tx };
                        None
                    }
                }
                PendingLaunchState::Reserved
                | PendingLaunchState::Published { .. }
                | PendingLaunchState::EmittingBegin { .. }
                | PendingLaunchState::Begun { .. }
                | PendingLaunchState::Cleaning
                | PendingLaunchState::WatchingProcess
                | PendingLaunchState::Complete => return Err(PendingLaunchError),
            }
        };
        if let Some(decision) = decision {
            let Some(decision_tx) = decision_tx else {
                return Err(PendingLaunchError);
            };
            let _ = decision_tx.send(decision);
        }
        Ok(decision_rx)
    }

    pub(super) fn request_cleanup(&self) {
        let (decision, suppress_events) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, PendingLaunchState::Complete) {
                PendingLaunchState::Reserved => {
                    self.completed.cancel();
                    (None, true)
                }
                PendingLaunchState::Launching { .. } => {
                    *state = PendingLaunchState::Launching {
                        cleanup_requested: true,
                    };
                    (None, true)
                }
                PendingLaunchState::Published { decision_tx } => {
                    *state = PendingLaunchState::Cleaning;
                    (
                        Some((
                            decision_tx,
                            PendingLaunchDecision::Cleanup {
                                terminal_event: PendingLaunchTerminalEvent::Suppress,
                            },
                        )),
                        true,
                    )
                }
                PendingLaunchState::EmittingBegin {
                    decision_tx,
                    origin_abandoned,
                    ..
                } => {
                    *state = PendingLaunchState::EmittingBegin {
                        decision_tx,
                        cleanup_requested: true,
                        origin_abandoned,
                    };
                    (None, false)
                }
                PendingLaunchState::Begun { decision_tx } => {
                    *state = PendingLaunchState::Cleaning;
                    (
                        Some((
                            decision_tx,
                            PendingLaunchDecision::Cleanup {
                                terminal_event: PendingLaunchTerminalEvent::Emit,
                            },
                        )),
                        false,
                    )
                }
                previous_state @ (PendingLaunchState::Cleaning
                | PendingLaunchState::WatchingProcess
                | PendingLaunchState::Complete) => {
                    *state = previous_state;
                    (None, false)
                }
            }
        };
        if suppress_events {
            self.suppress_events();
        }
        if let Some((decision_tx, decision)) = decision {
            let _ = decision_tx.send(decision);
        }
    }

    fn begin_event(self: &Arc<Self>) -> Result<PendingBeginEventGuard, PendingLaunchError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, PendingLaunchState::Complete) {
            PendingLaunchState::Published { decision_tx } => {
                *state = PendingLaunchState::EmittingBegin {
                    decision_tx,
                    cleanup_requested: false,
                    origin_abandoned: false,
                };
                Ok(PendingBeginEventGuard {
                    launch: Arc::clone(self),
                    committed: false,
                })
            }
            previous_state => {
                *state = previous_state;
                Err(PendingLaunchError)
            }
        }
    }

    fn commit_begin_event(&self) -> Result<(), PendingLaunchError> {
        let decision_tx = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, PendingLaunchState::Complete) {
                PendingLaunchState::EmittingBegin {
                    decision_tx,
                    cleanup_requested,
                    origin_abandoned,
                } => {
                    // The emitting task calls this only after the canonical
                    // Begin future has completed. Publish visibility while the
                    // state lock still excludes a racing cleanup decision.
                    self.event_visibility
                        .send_replace(PendingLaunchEventVisibility::Emit);
                    if cleanup_requested || origin_abandoned {
                        *state = PendingLaunchState::Cleaning;
                        Some(decision_tx)
                    } else {
                        *state = PendingLaunchState::Begun { decision_tx };
                        None
                    }
                }
                previous_state => {
                    *state = previous_state;
                    return Err(PendingLaunchError);
                }
            }
        };
        if let Some(decision_tx) = decision_tx
            && decision_tx
                .send(PendingLaunchDecision::Cleanup {
                    terminal_event: PendingLaunchTerminalEvent::Emit,
                })
                .is_err()
        {
            self.complete(PendingLaunchOutcome::default());
            return Err(PendingLaunchError);
        }
        Ok(())
    }

    fn abandon_begin_event(&self) {
        let decision_tx = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, PendingLaunchState::Complete) {
                PendingLaunchState::EmittingBegin { decision_tx, .. } => {
                    *state = PendingLaunchState::Cleaning;
                    Some(decision_tx)
                }
                previous_state => {
                    *state = previous_state;
                    None
                }
            }
        };
        self.suppress_events();
        if let Some(decision_tx) = decision_tx {
            let _ = decision_tx.send(PendingLaunchDecision::Cleanup {
                terminal_event: PendingLaunchTerminalEvent::Suppress,
            });
        }
    }

    fn watch_process(&self) -> Result<(), PendingLaunchError> {
        let decision_tx = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, PendingLaunchState::WatchingProcess) {
                PendingLaunchState::Begun { decision_tx } => decision_tx,
                previous_state => {
                    *state = previous_state;
                    return Err(PendingLaunchError);
                }
            }
        };
        if decision_tx
            .send(PendingLaunchDecision::WatchProcess)
            .is_err()
        {
            self.complete(PendingLaunchOutcome::default());
            return Err(PendingLaunchError);
        }
        Ok(())
    }

    fn abandon_origin(&self) {
        let (decision, suppress_events) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, PendingLaunchState::Complete) {
                PendingLaunchState::Reserved | PendingLaunchState::Launching { .. } => {
                    self.completed.cancel();
                    (None, true)
                }
                PendingLaunchState::Published { decision_tx } => {
                    *state = PendingLaunchState::Cleaning;
                    (
                        Some((
                            decision_tx,
                            PendingLaunchDecision::Cleanup {
                                terminal_event: PendingLaunchTerminalEvent::Suppress,
                            },
                        )),
                        true,
                    )
                }
                PendingLaunchState::EmittingBegin {
                    decision_tx,
                    cleanup_requested,
                    ..
                } => {
                    *state = PendingLaunchState::EmittingBegin {
                        decision_tx,
                        cleanup_requested,
                        origin_abandoned: true,
                    };
                    (None, false)
                }
                PendingLaunchState::Begun { decision_tx } => {
                    *state = PendingLaunchState::Cleaning;
                    (
                        Some((
                            decision_tx,
                            PendingLaunchDecision::Cleanup {
                                terminal_event: PendingLaunchTerminalEvent::Emit,
                            },
                        )),
                        false,
                    )
                }
                previous_state @ (PendingLaunchState::Cleaning
                | PendingLaunchState::WatchingProcess
                | PendingLaunchState::Complete) => {
                    *state = previous_state;
                    (None, false)
                }
            }
        };
        if suppress_events {
            self.suppress_events();
        }
        if let Some((decision_tx, decision)) = decision {
            let _ = decision_tx.send(decision);
        }
    }

    pub(super) fn suppress_events(&self) {
        self.event_visibility
            .send_replace(PendingLaunchEventVisibility::Suppress);
    }

    pub(super) fn complete(&self, outcome: PendingLaunchOutcome) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outcome;
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PendingLaunchState::Complete;
        self.completed.cancel();
    }

    pub(super) fn is_complete(&self) -> bool {
        self.completed.is_cancelled()
    }

    pub(super) async fn wait(&self) -> PendingLaunchOutcome {
        self.completed.cancelled().await;
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Debug)]
pub(super) struct PendingLaunchGuard {
    launch: Arc<PendingLaunch>,
    handed_off: bool,
}

impl PendingLaunchGuard {
    pub(super) fn launch(&self) -> &Arc<PendingLaunch> {
        &self.launch
    }

    pub(super) fn watch_process(&mut self) -> Result<(), PendingLaunchError> {
        self.launch.watch_process()?;
        self.handed_off = true;
        Ok(())
    }

    /// Publishes the canonical start event from a manager-owned task. If the
    /// originating tool future is dropped while publication is in progress,
    /// the task still commits the start before cleanup emits its matching end.
    pub(super) fn spawn_manager_owned_begin<Fut>(
        &self,
        emit: Fut,
    ) -> Result<oneshot::Receiver<Result<(), PendingLaunchError>>, PendingLaunchError>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let begin_event_guard = self.launch.begin_event()?;
        let (result_tx, result_rx) = oneshot::channel();
        self.launch.spawn_manager_task(async move {
            emit.await;
            let result = begin_event_guard.commit();
            let _ = result_tx.send(result);
        });
        Ok(result_rx)
    }

    /// Keeps launch ownership in a detached task until it either publishes a
    /// process supervisor or reports a definitive start failure. Dropping the
    /// caller-side receiver cannot abandon the in-flight backend start.
    pub(super) fn spawn_manager_owned<T, F, Fut>(self, launch: F) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let pending_launch = Arc::clone(&self.launch);
        let (result_tx, result_rx) = oneshot::channel();
        pending_launch.spawn_manager_task(async move {
            let result = launch(self).await;
            let _ = result_tx.send(result);
        });
        result_rx
    }
}

struct PendingBeginEventGuard {
    launch: Arc<PendingLaunch>,
    committed: bool,
}

impl PendingBeginEventGuard {
    fn commit(mut self) -> Result<(), PendingLaunchError> {
        self.launch.commit_begin_event()?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PendingBeginEventGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.launch.abandon_begin_event();
        }
    }
}

impl Drop for PendingLaunchGuard {
    fn drop(&mut self) {
        if !self.handed_off {
            self.launch.abandon_origin();
        }
    }
}

#[derive(Debug)]
pub(super) struct PendingLaunchError;
