use super::process::NoopSpawnLifecycle;
use super::process::UnifiedExecProcess;
use super::process_state::ProcessState;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_errors: Mutex<VecDeque<String>>,
    termination_count: Arc<AtomicUsize>,
    termination_started: Option<Arc<Notify>>,
    termination_allowed: Option<Arc<Notify>>,
    termination_call_dropped: Option<Arc<AtomicBool>>,
    wake_tx: watch::Sender<u64>,
}

struct TerminationCallDropGuard(Arc<AtomicBool>);

impl Drop for TerminationCallDropGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

enum MockTerminationBehavior {
    Immediate,
    Gated {
        started: Arc<Notify>,
        allowed: Arc<Notify>,
    },
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        let _drop_guard = self
            .termination_call_dropped
            .as_ref()
            .map(|dropped| TerminationCallDropGuard(Arc::clone(dropped)));
        self.termination_count.fetch_add(1, Ordering::Relaxed);
        if let Some(termination_started) = self.termination_started.as_ref() {
            termination_started.notify_one();
        }
        if let Some(termination_allowed) = self.termination_allowed.as_ref() {
            termination_allowed.notified().await;
        }
        if let Some(message) = self.terminate_errors.lock().await.pop_front() {
            return Err(ExecServerError::Protocol(message));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

pub(super) async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
) -> UnifiedExecProcess {
    remote_process_with_termination_count(write_status, terminate_error, sandbox_type)
        .await
        .0
}

pub(super) async fn remote_process_with_termination_count(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
) -> (UnifiedExecProcess, Arc<AtomicUsize>) {
    let (process, termination_count, _, _) = remote_process_with_termination_control(
        write_status,
        terminate_error,
        sandbox_type,
        MockTerminationBehavior::Immediate,
        /*termination_call_dropped*/ None,
    )
    .await;
    (process, termination_count)
}

pub(super) async fn remote_process_with_gated_termination(
    write_status: WriteStatus,
    sandbox_type: codex_sandboxing::SandboxType,
) -> (
    UnifiedExecProcess,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Notify>,
) {
    let termination_started = Arc::new(Notify::new());
    let termination_allowed = Arc::new(Notify::new());
    let (process, termination_count, termination_started, termination_allowed) =
        remote_process_with_termination_control(
            write_status,
            /*terminate_error*/ None,
            sandbox_type,
            MockTerminationBehavior::Gated {
                started: Arc::clone(&termination_started),
                allowed: Arc::clone(&termination_allowed),
            },
            /*termination_call_dropped*/ None,
        )
        .await;
    (
        process,
        termination_count,
        termination_started.expect("gated termination should report when it starts"),
        termination_allowed.expect("gated termination should wait for release"),
    )
}

pub(super) async fn remote_process_with_gated_termination_drop_probe(
    write_status: WriteStatus,
    sandbox_type: codex_sandboxing::SandboxType,
) -> (
    UnifiedExecProcess,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Notify>,
    Arc<AtomicBool>,
) {
    let termination_started = Arc::new(Notify::new());
    let termination_allowed = Arc::new(Notify::new());
    let termination_call_dropped = Arc::new(AtomicBool::new(false));
    let (process, termination_count, termination_started, termination_allowed) =
        remote_process_with_termination_control(
            write_status,
            /*terminate_error*/ None,
            sandbox_type,
            MockTerminationBehavior::Gated {
                started: Arc::clone(&termination_started),
                allowed: Arc::clone(&termination_allowed),
            },
            Some(Arc::clone(&termination_call_dropped)),
        )
        .await;
    (
        process,
        termination_count,
        termination_started.expect("gated termination should report when it starts"),
        termination_allowed.expect("gated termination should wait for release"),
        termination_call_dropped,
    )
}

async fn remote_process_with_termination_control(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
    termination_behavior: MockTerminationBehavior,
    termination_call_dropped: Option<Arc<AtomicBool>>,
) -> (
    UnifiedExecProcess,
    Arc<AtomicUsize>,
    Option<Arc<Notify>>,
    Option<Arc<Notify>>,
) {
    let (started, termination_count, termination_started, termination_allowed) =
        remote_started_with_termination_control(
            write_status,
            terminate_error,
            sandbox_type,
            termination_behavior,
            termination_call_dropped,
        );

    let process = UnifiedExecProcess::from_exec_server_started(started)
        .await
        .expect("remote process should start");
    (
        process,
        termination_count,
        termination_started,
        termination_allowed,
    )
}

fn remote_started_with_termination_control(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
    termination_behavior: MockTerminationBehavior,
    termination_call_dropped: Option<Arc<AtomicBool>>,
) -> (
    StartedExecProcess,
    Arc<AtomicUsize>,
    Option<Arc<Notify>>,
    Option<Arc<Notify>>,
) {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let termination_count = Arc::new(AtomicUsize::new(0));
    let (termination_started, termination_allowed) = match termination_behavior {
        MockTerminationBehavior::Immediate => (None, None),
        MockTerminationBehavior::Gated { started, allowed } => (Some(started), Some(allowed)),
    };
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_errors: Mutex::new(terminate_error.into_iter().collect()),
            termination_count: Arc::clone(&termination_count),
            termination_started: termination_started.clone(),
            termination_allowed: termination_allowed.clone(),
            termination_call_dropped,
            wake_tx,
        }),
        sandbox_type: Some(sandbox_type),
    };
    (
        started,
        termination_count,
        termination_started,
        termination_allowed,
    )
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(
        WriteStatus::UnknownProcess,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(
        WriteStatus::StdinClosed,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_remote_exit_preserves_resolved_failure_message() {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (_stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(1);
    let (_exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = Arc::new(
        UnifiedExecProcess::from_spawned(
            spawned,
            codex_sandboxing::SandboxType::None,
            Box::new(NoopSpawnLifecycle),
        )
        .await
        .expect("local process should start"),
    );
    let failure_entered = Arc::new(std::sync::Barrier::new(2));
    let failure_release = Arc::new(std::sync::Barrier::new(2));
    process.gate_next_failure_state_mutation_for_test(
        Arc::clone(&failure_entered),
        Arc::clone(&failure_release),
    );

    let process_for_failure = Arc::clone(&process);
    let failure = tokio::task::spawn_blocking(move || {
        process_for_failure.fail_and_terminate("resolved network denial".to_string());
    });
    tokio::task::spawn_blocking(move || failure_entered.wait())
        .await
        .expect("failure gate should be entered");

    let process_for_exit = Arc::clone(&process);
    let exit = tokio::task::spawn_blocking(move || process_for_exit.signal_exit(Some(7)));
    tokio::task::yield_now().await;
    assert!(
        !exit.is_finished(),
        "exit mutation must serialize behind failure publication"
    );
    tokio::task::spawn_blocking(move || failure_release.wait())
        .await
        .expect("failure gate should be released");
    failure.await.expect("failure task should finish");
    exit.await.expect("exit task should finish");

    assert_eq!(
        process.subscribe_state().borrow().clone(),
        ProcessState {
            has_exited: true,
            exit_code: Some(7),
            failure_message: Some("resolved network denial".to_string()),
            sandbox_denied: false,
        }
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_retries_after_failure_and_updates_state_on_success() {
    let (process, failed_then_retried_count) = remote_process_with_termination_count(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());
    assert_eq!(failed_then_retried_count.load(Ordering::Relaxed), 1);

    process
        .terminate_confirmed()
        .await
        .expect("a later termination attempt should retry the executor");

    assert!(process.has_exited());
    assert_eq!(failed_then_retried_count.load(Ordering::Relaxed), 2);

    let (process, termination_count) = remote_process_with_termination_count(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
    drop(process);
    tokio::task::yield_now().await;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn aborting_first_confirmed_termination_does_not_strand_followers() {
    let (process, termination_count, termination_started, termination_allowed) =
        remote_process_with_gated_termination(
            WriteStatus::Accepted,
            codex_sandboxing::SandboxType::None,
        )
        .await;
    let process = Arc::new(process);
    let first_process = Arc::clone(&process);
    let first = tokio::spawn(async move { first_process.terminate_confirmed().await });
    termination_started.notified().await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("first caller should be aborted")
            .is_cancelled()
    );

    let follower_process = Arc::clone(&process);
    let follower = tokio::spawn(async move { follower_process.terminate_confirmed().await });
    tokio::task::yield_now().await;
    assert!(!follower.is_finished());
    termination_allowed.notify_one();
    tokio::time::timeout(Duration::from_secs(1), follower)
        .await
        .expect("manager-owned termination should outlive its first caller")
        .expect("follower task should complete")
        .expect("termination should succeed");

    drop(process);
    tokio::task::yield_now().await;
    assert_eq!(termination_count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn unpolled_manager_owned_termination_resolves_followers() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;
    let process_id_lease = crate::unified_exec::ProcessIdLease::new(123);
    process.attach_process_id_lease(process_id_lease.clone());
    process_id_lease
        .pending_launch()
        .force_shutdown(Default::default())
        .await;

    let error = tokio::time::timeout(Duration::from_secs(1), process.terminate_confirmed())
        .await
        .expect("born-cancelled termination must resolve its attempt")
        .expect_err("an unpolled termination cannot report success");

    assert_eq!(
        error.to_string(),
        "Unified exec process failed: termination task ended before publishing a result"
    );
}

#[tokio::test]
async fn early_remote_denial_routes_stalled_drop_termination_through_pending_launch() {
    let termination_started = Arc::new(Notify::new());
    let termination_allowed = Arc::new(Notify::new());
    let termination_call_dropped = Arc::new(AtomicBool::new(false));
    let (started, _, _, _) = remote_started_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::LinuxSeccomp,
        MockTerminationBehavior::Gated {
            started: Arc::clone(&termination_started),
            allowed: Arc::clone(&termination_allowed),
        },
        Some(Arc::clone(&termination_call_dropped)),
    );
    let process_id_lease = crate::unified_exec::ProcessIdLease::new(123);
    let pending_launch = Arc::clone(process_id_lease.pending_launch());

    let error = async {
        let process = UnifiedExecProcess::from_exec_server_started_for_pending_launch(
            started,
            process_id_lease,
        )
        .await?;
        process.signal_exit(Some(1));
        process
            .check_for_sandbox_denial_with_text("permission denied")
            .await?;
        Ok::<_, UnifiedExecError>(process)
    }
    .await
    .expect_err("early sandbox denial should reject the remote process");
    assert!(matches!(error, UnifiedExecError::SandboxDenied { .. }));

    termination_started.notified().await;
    pending_launch.force_shutdown(Default::default()).await;
    assert!(
        termination_call_dropped.load(Ordering::Acquire),
        "pending-launch force must drop stalled termination from rejected startup"
    );
}

#[tokio::test]
async fn rejected_start_cleanup_polls_stalled_termination_before_cancelling() {
    let (
        process,
        termination_count,
        _termination_started,
        _termination_allowed,
        termination_call_dropped,
    ) = remote_process_with_gated_termination_drop_probe(
        WriteStatus::Accepted,
        codex_sandboxing::SandboxType::LinuxSeccomp,
    )
    .await;
    process.attach_process_id_lease(crate::unified_exec::ProcessIdLease::new(123));

    tokio::time::timeout(Duration::from_secs(1), process.terminate_rejected_startup())
        .await
        .expect("rejected-start cleanup must not await a stalled termination");

    assert_eq!(termination_count.load(Ordering::Acquire), 1);
    assert!(
        termination_call_dropped.load(Ordering::Acquire),
        "rejected-start cleanup must drop the polled termination future"
    );
    drop(process);
    tokio::task::yield_now().await;
    assert_eq!(termination_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn remote_process_preserves_executor_sandbox_type() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::LinuxSeccomp,
    )
    .await;

    assert_eq!(
        process.sandbox_type(),
        codex_sandboxing::SandboxType::LinuxSeccomp
    );
}
