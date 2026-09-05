#![allow(clippy::module_inception)]

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::ProcessSignal as ExecServerProcessSignal;
use codex_exec_server::ReadResponse as ExecReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteStatus;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::TruncationPolicy;
use codex_sandboxing::SandboxType;
use codex_sandboxing::is_likely_sandbox_denied;
use codex_sandboxing::record_filesystem_sandbox_violation;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use codex_utils_pty::SpawnedPty;

use super::ProcessIdLease;
use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UNIFIED_EXEC_OUTPUT_MAX_TOKENS;
use super::UnifiedExecError;
use super::async_watcher::TRAILING_OUTPUT_GRACE;
use super::head_tail_buffer::HeadTailBuffer;
use super::pending_launch::ManagerTaskHandle;
use super::process_state::ProcessState;

const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);
pub(crate) trait SpawnLifecycle: std::fmt::Debug + Send + Sync {
    /// Returns file descriptors that must stay open across the child `exec()`.
    ///
    /// The returned descriptors must already be valid in the parent process and
    /// stay valid until `after_spawn()` runs, which is the first point where
    /// the parent may release its copies.
    fn inherited_fds(&self) -> Vec<i32> {
        Vec::new()
    }

    fn after_spawn(&mut self) {}
}

pub(crate) type SpawnLifecycleHandle = Box<dyn SpawnLifecycle>;

#[derive(Debug, Default)]
/// Spawn lifecycle that performs no extra setup around process launch.
pub(crate) struct NoopSpawnLifecycle;

impl SpawnLifecycle for NoopSpawnLifecycle {}

/// Shared output state exposed to polling and streaming consumers.
#[derive(Clone)]
pub(crate) struct OutputHandles<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    pub(crate) output_buffer: Arc<Mutex<HeadTailBuffer<MAX_BYTES>>>,
    pub(crate) transcript: Arc<Mutex<HeadTailBuffer<MAX_BYTES>>>,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) output_closed: Arc<AtomicBool>,
    pub(crate) output_closed_notify: Arc<Notify>,
    pub(crate) cancellation_token: CancellationToken,
}

#[derive(Clone, Debug)]
pub(super) struct PublishedOutputChunk {
    pub(super) offset: usize,
    pub(super) bytes: Vec<u8>,
}

impl PublishedOutputChunk {
    pub(super) fn end_offset(&self) -> usize {
        self.offset.saturating_add(self.bytes.len())
    }
}

struct OutputTaskGuard {
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
}

impl Drop for OutputTaskGuard {
    fn drop(&mut self) {
        self.output_closed.store(true, Ordering::Release);
        self.output_closed_notify.notify_waiters();
    }
}

/// Transport-specific process handle used by unified exec.
enum ProcessHandle {
    Local(Box<ExecCommandSession>),
    ExecServer(Arc<dyn ExecProcess>),
}

enum ExecServerProcessOwnership {
    Standalone,
    PendingLaunch(ProcessIdLease),
}

struct TerminationState {
    current_attempt: StdMutex<Option<Arc<TerminationAttempt>>>,
}

struct TerminationAttempt {
    first_poll: CancellationToken,
    completed: CancellationToken,
    result: StdMutex<Option<Result<(), String>>>,
    task: StdMutex<Option<TerminationTaskHandle>>,
}

#[derive(Clone, Debug)]
enum TerminationTaskHandle {
    Managed(ManagerTaskHandle),
    Background(BackgroundTaskHandle),
}

impl TerminationTaskHandle {
    async fn cancel_and_wait(&self) {
        match self {
            Self::Managed(task) => task.cancel_and_wait().await,
            Self::Background(task) => task.abort_and_wait().await,
        }
    }
}

#[derive(Clone, Debug)]
struct BackgroundTaskHandle {
    abort_handle: tokio::task::AbortHandle,
    completed: CancellationToken,
}

impl BackgroundTaskHandle {
    fn spawn(task: impl Future<Output = ()> + Send + 'static) -> Self {
        let task = tokio::spawn(task);
        let abort_handle = task.abort_handle();
        let completed = CancellationToken::new();
        let completed_for_task = completed.clone();
        tokio::spawn(async move {
            let _ = task.await;
            completed_for_task.cancel();
        });
        Self {
            abort_handle,
            completed,
        }
    }

    fn abort(&self) {
        self.abort_handle.abort();
    }

    async fn abort_and_wait(&self) {
        self.abort();
        self.completed.cancelled().await;
    }
}

impl TerminationState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            current_attempt: StdMutex::new(None),
        })
    }

    fn begin_attempt(&self) -> TerminationAttemptRole {
        let mut current_attempt = self
            .current_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(attempt) = current_attempt.as_ref() {
            let result = attempt.result();
            if result.as_ref().is_none_or(Result::is_ok) {
                return TerminationAttemptRole::Join(Arc::clone(attempt));
            }
        }
        let attempt = Arc::new(TerminationAttempt::new());
        *current_attempt = Some(Arc::clone(&attempt));
        TerminationAttemptRole::Start(attempt)
    }
}

impl TerminationAttempt {
    fn new() -> Self {
        Self {
            first_poll: CancellationToken::new(),
            completed: CancellationToken::new(),
            result: StdMutex::new(None),
            task: StdMutex::new(None),
        }
    }

    fn set_task(&self, task: TerminationTaskHandle) {
        *self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }

    fn task(&self) -> Option<TerminationTaskHandle> {
        self.task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn finish(&self, result: Result<(), String>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        self.completed.cancel();
    }

    async fn wait(&self) -> Result<(), String> {
        self.completed.cancelled().await;
        let result = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(result) = result else {
            unreachable!("started termination should publish exactly one result");
        };
        result
    }

    async fn wait_until_first_poll_or_completion(&self) {
        tokio::select! {
            biased;
            _ = self.first_poll.cancelled() => {}
            _ = self.completed.cancelled() => {}
        }
    }

    fn result(&self) -> Option<Result<(), String>> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

enum TerminationAttemptRole {
    Start(Arc<TerminationAttempt>),
    Join(Arc<TerminationAttempt>),
}

impl TerminationAttemptRole {
    fn attempt(&self) -> &Arc<TerminationAttempt> {
        match self {
            Self::Start(attempt) | Self::Join(attempt) => attempt,
        }
    }
}

struct TerminationTaskGuard {
    attempt: Arc<TerminationAttempt>,
    completed: bool,
}

impl TerminationTaskGuard {
    fn new(attempt: Arc<TerminationAttempt>) -> Self {
        Self {
            attempt,
            completed: false,
        }
    }

    fn complete(mut self, result: Result<(), String>) {
        self.completed = true;
        self.attempt.finish(result);
    }
}

impl Drop for TerminationTaskGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.attempt.finish(Err(
                "termination task ended before publishing a result".to_string()
            ));
        }
    }
}

/// Unified wrapper over directly spawned PTY sessions and exec-server-backed
/// processes.
pub(crate) struct UnifiedExecProcess {
    process_handle: ProcessHandle,
    output_tx: broadcast::Sender<PublishedOutputChunk>,
    output: OutputHandles,
    output_drained: Arc<Notify>,
    interaction_lock: Arc<Mutex<()>>,
    wait_process_lock: Arc<Mutex<()>>,
    operation_lifetime: Arc<()>,
    termination: Arc<TerminationState>,
    terminate_on_drop: AtomicBool,
    process_id_lease: OnceLock<ProcessIdLease>,
    state_tx: watch::Sender<ProcessState>,
    state_rx: watch::Receiver<ProcessState>,
    output_task: Option<BackgroundTaskHandle>,
    sandbox_type: SandboxType,
    timed_out: AtomicBool,
    _spawn_lifecycle: Option<SpawnLifecycleHandle>,
    #[cfg(test)]
    failure_state_mutation_gate:
        StdMutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
}

impl std::fmt::Debug for UnifiedExecProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecProcess")
            .field("has_exited", &self.has_exited())
            .field("exit_code", &self.exit_code())
            .field("sandbox_type", &self.sandbox_type)
            .finish_non_exhaustive()
    }
}

impl UnifiedExecProcess {
    fn new(
        process_handle: ProcessHandle,
        sandbox_type: SandboxType,
        spawn_lifecycle: Option<SpawnLifecycleHandle>,
    ) -> Self {
        let output = OutputHandles {
            output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
            transcript: Arc::new(Mutex::new(HeadTailBuffer::default())),
            output_notify: Arc::new(Notify::new()),
            output_closed: Arc::new(AtomicBool::new(false)),
            output_closed_notify: Arc::new(Notify::new()),
            cancellation_token: CancellationToken::new(),
        };
        let output_drained = Arc::new(Notify::new());
        let (output_tx, _) = broadcast::channel(64);
        let (state_tx, state_rx) = watch::channel(ProcessState::default());

        Self {
            process_handle,
            output_tx,
            output,
            output_drained,
            interaction_lock: Arc::new(Mutex::new(())),
            wait_process_lock: Arc::new(Mutex::new(())),
            operation_lifetime: Arc::new(()),
            termination: TerminationState::new(),
            terminate_on_drop: AtomicBool::new(true),
            process_id_lease: OnceLock::new(),
            state_tx,
            state_rx,
            output_task: None,
            sandbox_type,
            timed_out: AtomicBool::new(false),
            _spawn_lifecycle: spawn_lifecycle,
            #[cfg(test)]
            failure_state_mutation_gate: StdMutex::new(None),
        }
    }

    pub(super) async fn write(&self, data: &[u8]) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .writer_sender()
                .send(data.to_vec())
                .await
                .map_err(|_| UnifiedExecError::WriteToStdin),
            ProcessHandle::ExecServer(process_handle) => {
                match process_handle.write(data.to_vec()).await {
                    Ok(response) => match response.status {
                        WriteStatus::Accepted => Ok(()),
                        WriteStatus::UnknownProcess | WriteStatus::StdinClosed => {
                            self.state_tx.send_modify(|state| {
                                let exit_code = state.exit_code;
                                state.mark_exited(exit_code);
                            });
                            self.output.cancellation_token.cancel();
                            Err(UnifiedExecError::WriteToStdin)
                        }
                        WriteStatus::Starting => Err(UnifiedExecError::WriteToStdin),
                    },
                    Err(err) => Err(UnifiedExecError::process_failed(err.to_string())),
                }
            }
        }
    }

    pub(super) fn output_handles(&self) -> &OutputHandles {
        &self.output
    }

    pub(super) fn output_receiver(&self) -> tokio::sync::broadcast::Receiver<PublishedOutputChunk> {
        self.output_tx.subscribe()
    }

    pub(super) fn output_transcript(&self) -> Arc<Mutex<HeadTailBuffer>> {
        Arc::clone(&self.output.transcript)
    }

    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.output.cancellation_token.clone()
    }

    pub(super) fn output_drained_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.output_drained)
    }

    pub(super) fn interaction_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.interaction_lock)
    }

    pub(super) fn wait_process_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.wait_process_lock)
    }

    pub(super) fn acquire_operation_lease(&self) -> ProcessOperationLease {
        ProcessOperationLease {
            _lifetime: Arc::clone(&self.operation_lifetime),
        }
    }

    pub(super) fn has_active_operation(&self) -> bool {
        Arc::strong_count(&self.operation_lifetime) > 1
    }

    pub(super) fn attach_process_id_lease(&self, process_id_lease: ProcessIdLease) {
        let _ = self.process_id_lease.set(process_id_lease);
    }

    pub(super) fn process_id_lease(&self) -> Option<&ProcessIdLease> {
        self.process_id_lease.get()
    }

    pub(super) fn subscribe_state(&self) -> watch::Receiver<ProcessState> {
        self.state_rx.clone()
    }

    pub(super) fn has_exited(&self) -> bool {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => state.has_exited || process_handle.has_exited(),
            ProcessHandle::ExecServer(_) => state.has_exited,
        }
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        if self.timed_out() {
            return Some(124);
        }
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                state.exit_code.or_else(|| process_handle.exit_code())
            }
            ProcessHandle::ExecServer(_) => state.exit_code,
        }
    }

    pub(super) fn mark_timed_out(&self) {
        self.timed_out.store(true, Ordering::Release);
    }

    pub(super) fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }

    fn finish_termination(&self) {
        self.output.cancellation_token.cancel();
        if let Some(output_task) = &self.output_task {
            output_task.abort();
        }
    }

    fn start_termination(&self) -> Arc<TerminationAttempt> {
        let attempt_role = self.termination.begin_attempt();
        let attempt = Arc::clone(attempt_role.attempt());
        let TerminationAttemptRole::Start(_) = attempt_role else {
            return attempt;
        };
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                process_handle.terminate();
                attempt.finish(Ok(()));
            }
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                // Construct the guard before spawning. A manager task can be
                // born cancelled and drop its future without polling it once;
                // the captured guard must still resolve followers in that case.
                let completion = TerminationTaskGuard::new(Arc::clone(&attempt));
                let first_poll = attempt.first_poll.clone();
                let termination = async move {
                    let mut termination = process_handle.terminate();
                    let result = std::future::poll_fn(|cx| {
                        let result = termination.as_mut().poll(cx);
                        first_poll.cancel();
                        result
                    })
                    .await
                    .map_err(|err| err.to_string());
                    completion.complete(result);
                };
                let termination_task = if let Some(process_id_lease) = self.process_id_lease() {
                    TerminationTaskHandle::Managed(
                        process_id_lease
                            .pending_launch()
                            .spawn_manager_task(termination),
                    )
                } else {
                    TerminationTaskHandle::Background(BackgroundTaskHandle::spawn(termination))
                };
                attempt.set_task(termination_task);
            }
        }
        attempt
    }

    pub(super) fn terminate(&self) {
        self.start_termination();
        self.finish_termination();
    }

    pub(super) async fn terminate_confirmed(&self) -> Result<(), UnifiedExecError> {
        let attempt = self.start_termination();
        let result = attempt.wait().await;
        result.map_err(UnifiedExecError::process_failed)?;
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(super) async fn interrupt(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .signal(PtyProcessSignal::Interrupt)
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
            ProcessHandle::ExecServer(process_handle) => process_handle
                .signal(ExecServerProcessSignal::Interrupt)
                .await
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
        }
    }

    pub(super) fn fail_and_terminate(&self, message: String) {
        #[cfg(test)]
        let failure_state_mutation_gate = self
            .failure_state_mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.state_tx.send_modify(move |state| {
            #[cfg(test)]
            if let Some((entered, release)) = failure_state_mutation_gate {
                entered.wait();
                release.wait();
            }
            state.fail_if_absent(message);
        });
        self.terminate();
    }

    #[cfg(test)]
    pub(super) fn gate_next_failure_state_mutation_for_test(
        &self,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    ) {
        *self
            .failure_state_mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((entered, release));
    }

    pub(super) fn mark_termination_failure(&self, message: String) {
        self.state_tx
            .send_modify(move |state| state.fail_if_absent(message));
        self.signal_exit(self.exit_code());
        self.finish_termination();
    }

    pub(super) async fn force_shutdown_cleanup(&self, failure_message: &str) {
        if self.has_exited() {
            self.finish_termination();
        } else {
            self.mark_termination_failure(failure_message.to_string());
        }
        if let Some(output_task) = &self.output_task {
            output_task.abort_and_wait().await;
        }
        self.cancel_termination_task_and_wait().await;
    }

    async fn cancel_termination_task_and_wait(&self) {
        let termination_task = self
            .termination
            .current_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|attempt| attempt.task());
        if let Some(termination_task) = termination_task {
            termination_task.cancel_and_wait().await;
        }
    }

    pub(super) async fn terminate_rejected_startup(&self) {
        debug_assert!(self.process_id_lease().is_some());
        let attempt = self.start_termination();
        self.finish_termination();
        // Let best-effort remote cleanup enqueue its request before severing a
        // stalled call. Cancelling a born-unpolled manager task would otherwise
        // make a rejected start look cleaned up without notifying the executor.
        attempt.wait_until_first_poll_or_completion().await;
        self.cancel_termination_task_and_wait().await;
        self.terminate_on_drop.store(false, Ordering::Release);
    }

    async fn snapshot_output(&self) -> Vec<u8> {
        let guard = self.output.output_buffer.lock().await;
        guard.to_bytes()
    }

    pub(crate) fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    pub(super) fn failure_message(&self) -> Option<String> {
        self.state_rx.borrow().failure_message.clone()
    }

    pub(super) async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            self.output.output_notify.notified(),
        )
        .await;

        let aggregated = self.snapshot_output().await;
        let aggregated_text = String::from_utf8_lossy(&aggregated);
        self.check_for_sandbox_denial_with_text(aggregated_text.as_ref())
            .await?;

        Ok(())
    }

    pub(super) async fn check_for_sandbox_denial_with_text(
        &self,
        text: &str,
    ) -> Result<(), UnifiedExecError> {
        let executor_reported_denial = self.state_rx.borrow().sandbox_denied;
        let sandbox_type = self.sandbox_type();
        if !self.has_exited() || (!executor_reported_denial && sandbox_type == SandboxType::None) {
            return Ok(());
        }

        let exit_code = self.exit_code().unwrap_or(-1);
        let exec_output = ExecToolCallOutput {
            exit_code,
            stderr: StreamOutput::new(text.to_string()),
            aggregated_output: StreamOutput::new(text.to_string()),
            ..Default::default()
        };
        let likely_sandbox_denial = is_likely_sandbox_denied(sandbox_type, &exec_output);
        if likely_sandbox_denial {
            record_filesystem_sandbox_violation(sandbox_type, &exec_output);
        }
        if executor_reported_denial || likely_sandbox_denial {
            let snippet = formatted_truncate_text(
                text,
                TruncationPolicy::Tokens(UNIFIED_EXEC_OUTPUT_MAX_TOKENS),
            );
            let message = if snippet.is_empty() {
                format!("Process exited with code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message, exec_output));
        }
        Ok(())
    }

    pub(super) async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
        spawn_lifecycle: SpawnLifecycleHandle,
    ) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session: process_handle,
            stdout_rx,
            stderr_rx,
            mut exit_rx,
        } = spawned;
        let output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
        let mut managed = Self::new(
            ProcessHandle::Local(Box::new(process_handle)),
            sandbox_type,
            Some(spawn_lifecycle),
        );
        managed.output_task = Some(Self::spawn_local_output_task(
            output_rx,
            managed.output_handles().clone(),
            managed.output_tx.clone(),
        ));

        match exit_rx.try_recv() {
            Ok(exit_code) => {
                managed.signal_exit(Some(exit_code));
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Closed) => {
                managed.signal_exit(/*exit_code*/ None);
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Empty) => {}
        }

        if let Ok(exit_result) = tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, &mut exit_rx).await {
            managed.signal_exit(exit_result.ok());
            managed.check_for_sandbox_denial().await?;
            return Ok(managed);
        }

        tokio::spawn({
            let state_tx = managed.state_tx.clone();
            let cancellation_token = managed.output.cancellation_token.clone();
            async move {
                let exit_code = exit_rx.await.ok();
                state_tx.send_modify(|state| state.mark_exited(exit_code));
                cancellation_token.cancel();
            }
        });

        Ok(managed)
    }

    pub(super) async fn from_exec_server_started(
        started: StartedExecProcess,
    ) -> Result<Self, UnifiedExecError> {
        Self::from_exec_server_started_with_ownership(
            started,
            ExecServerProcessOwnership::Standalone,
        )
        .await
    }

    pub(super) async fn from_exec_server_started_for_pending_launch(
        started: StartedExecProcess,
        process_id_lease: ProcessIdLease,
    ) -> Result<Self, UnifiedExecError> {
        Self::from_exec_server_started_with_ownership(
            started,
            ExecServerProcessOwnership::PendingLaunch(process_id_lease),
        )
        .await
    }

    async fn from_exec_server_started_with_ownership(
        started: StartedExecProcess,
        ownership: ExecServerProcessOwnership,
    ) -> Result<Self, UnifiedExecError> {
        let process_handle = ProcessHandle::ExecServer(Arc::clone(&started.process));
        // Older peers do not report this field. In that case, skip local
        // classification rather than attributing a violation to a guessed backend.
        let sandbox_type = started.sandbox_type.unwrap_or(SandboxType::None);
        let mut managed = Self::new(process_handle, sandbox_type, /*spawn_lifecycle*/ None);
        let manager_owned = match ownership {
            ExecServerProcessOwnership::Standalone => false,
            ExecServerProcessOwnership::PendingLaunch(process_id_lease) => {
                managed.attach_process_id_lease(process_id_lease);
                true
            }
        };
        let output_handles = managed.output_handles().clone();
        managed.output_task = Some(Self::spawn_exec_server_output_task(
            started,
            output_handles,
            managed.output_tx.clone(),
            managed.state_tx.clone(),
        ));

        let mut state_rx = managed.state_rx.clone();
        if tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, async {
            loop {
                let state = state_rx.borrow().clone();
                if state.has_exited || state.failure_message.is_some() {
                    break;
                }
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
            && let Err(error) = managed.check_for_sandbox_denial().await
        {
            // A rejected remote process still needs best-effort executor
            // cleanup. Start it while the pending-launch owner is available,
            // then sever that attempt before a sandbox retry can move on and
            // eventually release the reservation.
            if manager_owned {
                managed.terminate_rejected_startup().await;
            } else {
                managed.terminate();
            }
            return Err(error);
        }

        Ok(managed)
    }

    fn spawn_exec_server_output_task(
        started: StartedExecProcess,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<PublishedOutputChunk>,
        state_tx: watch::Sender<ProcessState>,
    ) -> BackgroundTaskHandle {
        let OutputHandles {
            output_buffer,
            transcript,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
        } = output_handles;
        let process = started.process;
        let mut events = process.subscribe_events();
        BackgroundTaskHandle::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            let mut last_seq: u64 = 0;
            let mut exit_grace_deadline = None;
            loop {
                let next_event = if let Some(deadline) = exit_grace_deadline {
                    match tokio::time::timeout_at(deadline, events.recv()).await {
                        Ok(event) => event,
                        Err(_) => break,
                    }
                } else {
                    events.recv().await
                };
                let event = match next_event {
                    Ok(event) => Some(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => {
                        state_tx.send_modify(|state| {
                            state.fail_if_absent(
                                "exec-server process event stream closed".to_string(),
                            );
                        });
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                };
                let event_seq = event.as_ref().and_then(|event| match event {
                    ExecProcessEvent::Output(chunk) => Some(chunk.seq),
                    ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq } => {
                        Some(*seq)
                    }
                    ExecProcessEvent::Failed(_) => None,
                });
                let missing_sandbox_denial = matches!(
                    event.as_ref(),
                    Some(ExecProcessEvent::Exited {
                        sandbox_denied: None,
                        ..
                    })
                );
                if event.is_none()
                    || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1))
                    || missing_sandbox_denial
                {
                    let response = match process
                        .read(
                            Some(last_seq),
                            /*max_bytes*/ None,
                            /*wait_ms*/ Some(0),
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(err) => {
                            state_tx.send_modify(|state| state.fail_if_absent(err.to_string()));
                            output_closed.store(true, Ordering::Release);
                            output_closed_notify.notify_waiters();
                            cancellation_token.cancel();
                            break;
                        }
                    };
                    let ExecReadResponse {
                        chunks,
                        next_seq,
                        exited,
                        exit_code,
                        closed,
                        failure,
                        sandbox_denied,
                    } = response;
                    for chunk in chunks.into_iter().filter(|chunk| chunk.seq > last_seq) {
                        let bytes = chunk.chunk.into_inner();
                        let mut transcript_guard = transcript.lock().await;
                        let offset = transcript_guard.total_bytes();
                        transcript_guard.push_chunk(&bytes);
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        let _ = output_tx.send(PublishedOutputChunk { offset, bytes });
                        drop(guard);
                        drop(transcript_guard);
                        output_notify.notify_waiters();
                    }
                    last_seq = last_seq.max(next_seq.saturating_sub(1));
                    if let Some(message) = failure {
                        state_tx.send_modify(|state| state.fail_if_absent(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    if sandbox_denied || exited {
                        state_tx.send_modify(|state| {
                            state.sandbox_denied |= sandbox_denied;
                            if exited {
                                state.mark_exited(exit_code);
                            }
                        });
                        if exited {
                            cancellation_token.cancel();
                            exit_grace_deadline.get_or_insert_with(|| {
                                tokio::time::Instant::now() + TRAILING_OUTPUT_GRACE
                            });
                        }
                    }
                    if closed {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    continue;
                }

                let Some(event) = event else {
                    continue;
                };
                match event {
                    ExecProcessEvent::Output(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        last_seq = chunk.seq;
                        let bytes = chunk.chunk.into_inner();
                        let mut transcript_guard = transcript.lock().await;
                        let offset = transcript_guard.total_bytes();
                        transcript_guard.push_chunk(&bytes);
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        let _ = output_tx.send(PublishedOutputChunk { offset, bytes });
                        drop(guard);
                        drop(transcript_guard);
                        output_notify.notify_waiters();
                    }
                    ExecProcessEvent::Exited {
                        seq,
                        exit_code,
                        sandbox_denied,
                    } => {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                        state_tx.send_modify(|state| {
                            state.sandbox_denied |= sandbox_denied.unwrap_or(false);
                            state.mark_exited(Some(exit_code));
                        });
                        cancellation_token.cancel();
                        exit_grace_deadline.get_or_insert_with(|| {
                            tokio::time::Instant::now() + TRAILING_OUTPUT_GRACE
                        });
                    }
                    ExecProcessEvent::Closed { seq } => {
                        if seq <= last_seq {
                            continue;
                        }
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    ExecProcessEvent::Failed(message) => {
                        state_tx.send_modify(|state| state.fail_if_absent(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
        })
    }

    fn spawn_local_output_task(
        mut receiver: tokio::sync::broadcast::Receiver<Vec<u8>>,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<PublishedOutputChunk>,
    ) -> BackgroundTaskHandle {
        let OutputHandles {
            output_buffer,
            transcript,
            output_notify,
            output_closed,
            output_closed_notify,
            ..
        } = output_handles;
        BackgroundTaskHandle::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            loop {
                match receiver.recv().await {
                    Ok(chunk) => {
                        let mut transcript_guard = transcript.lock().await;
                        let offset = transcript_guard.total_bytes();
                        transcript_guard.push_chunk(&chunk);
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&chunk);
                        let _ = output_tx.send(PublishedOutputChunk {
                            offset,
                            bytes: chunk,
                        });
                        drop(guard);
                        drop(transcript_guard);
                        output_notify.notify_waiters();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        break;
                    }
                };
            }
        })
    }

    pub(super) fn signal_exit(&self, exit_code: Option<i32>) {
        self.state_tx
            .send_modify(|state| state.mark_exited(exit_code));
        self.output.cancellation_token.cancel();
    }
}

pub(super) struct ProcessOperationLease {
    _lifetime: Arc<()>,
}

impl Drop for UnifiedExecProcess {
    fn drop(&mut self) {
        if self.terminate_on_drop.load(Ordering::Acquire) {
            self.terminate();
        } else {
            self.finish_termination();
        }
    }
}
