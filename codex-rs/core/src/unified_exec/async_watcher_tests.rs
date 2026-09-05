use std::sync::Arc;

use super::Buffer;
use super::EmitBarrier;
use super::Emitter;
use super::TRAILING_OUTPUT_GRACE;
use super::UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::pending_launch::PendingLaunch;
use crate::unified_exec::pending_launch::PendingLaunchDecision;
use crate::unified_exec::pending_launch::PendingLaunchEventVisibility;
use crate::unified_exec::pending_launch::PendingLaunchTerminalEvent;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::PublishedOutputChunk;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;

use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

struct StreamingOutputHarness {
    process: Arc<UnifiedExecProcess>,
    stdout_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_tx: tokio::sync::oneshot::Sender<i32>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    context: UnifiedExecContext,
    rx_event: async_channel::Receiver<Event>,
}

async fn streaming_output_harness() -> anyhow::Result<StreamingOutputHarness> {
    let pending_launch = PendingLaunch::new();
    pending_launch.emit_events_for_test();
    streaming_output_harness_with_pending_launch(pending_launch).await
}

async fn streaming_output_harness_with_pending_launch(
    pending_launch: Arc<PendingLaunch>,
) -> anyhow::Result<StreamingOutputHarness> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
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
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?,
    );
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "streaming-output-test".to_string(),
    );
    let transcript = process.output_transcript();
    start_streaming_output(&process, &context, pending_launch).await;

    Ok(StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    })
}

#[tokio::test]
async fn terminal_event_includes_output_published_before_streaming_subscribes() -> anyhow::Result<()>
{
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
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
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?,
    );
    let output_observed = process.output_handles().output_notify.notified();
    tokio::pin!(output_observed);
    stdout_tx
        .send(b"FAST-BEFORE-SUBSCRIBE".to_vec())
        .expect("send early output");
    (&mut output_observed).await;

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        tokio_util::sync::CancellationToken::new(),
        "fast-output-test".to_string(),
    );
    let transcript = process.output_transcript();
    let pending_launch = PendingLaunch::new();
    pending_launch.emit_events_for_test();
    start_streaming_output(&process, &context, pending_launch).await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let watcher = spawn_exit_watcher(
        Arc::clone(&process),
        session,
        turn,
        context.call_id,
        vec!["fast-command".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        /*network_denial_monitor*/ None,
        /*plugin_metrics_sidecar*/ None,
    );
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    watcher.await?;

    let event = rx_event.recv().await.expect("early output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "fast-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: b"FAST-BEFORE-SUBSCRIBE".to_vec(),
        }
    );
    let event = rx_event.recv().await.expect("command end event");
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("FAST-BEFORE-SUBSCRIBE")
    );
    Ok(())
}

#[tokio::test]
async fn streaming_output_replays_pre_subscription_output_after_begin() -> anyhow::Result<()> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
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
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?,
    );
    let output_observed = process.output_handles().output_notify.notified();
    tokio::pin!(output_observed);
    stdout_tx
        .send(b"FAST-BEFORE-BEGIN".to_vec())
        .expect("send early output");
    (&mut output_observed).await;

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        tokio_util::sync::CancellationToken::new(),
        "fast-output-before-begin-test".to_string(),
    );
    let pending_launch = PendingLaunch::new();
    let pending_launch_guard = pending_launch
        .begin()
        .expect("reserved launch should begin exactly once");
    let decision_rx = pending_launch_guard
        .launch()
        .publish()
        .expect("launched process should publish cleanup ownership");
    let transcript = process.output_transcript();
    start_streaming_output(&process, &context, Arc::clone(&pending_launch)).await;
    assert!(
        rx_event.try_recv().is_err(),
        "pre-subscription output must remain hidden until Begin"
    );

    let cwd = turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd()
        .clone();
    let begin_emitter = ToolEmitter::unified_exec(
        &["fast-command".to_string()],
        cwd,
        codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
        Some("123".to_string()),
        /*plugin_attribution*/ None,
    );
    let begin_rx = pending_launch_guard
        .spawn_manager_owned_begin(async move {
            begin_emitter
                .emit(
                    ToolEventCtx::new(
                        session.as_ref(),
                        turn.as_ref(),
                        "fast-output-before-begin-test",
                        /*turn_diff_tracker*/ None,
                    ),
                    ToolEventStage::Begin,
                )
                .await;
        })
        .expect("start event should be admitted");
    begin_rx
        .await
        .expect("manager-owned start event task should return")
        .expect("start event should commit");

    assert!(matches!(
        rx_event.recv().await.expect("receive item start").msg,
        EventMsg::ItemStarted(_)
    ));
    assert!(matches!(
        rx_event.recv().await.expect("receive legacy start").msg,
        EventMsg::ExecCommandBegin(_)
    ));
    let event = tokio::time::timeout(Duration::from_millis(25), rx_event.recv())
        .await
        .expect("pre-subscription output should replay after Begin")
        .expect("receive replayed output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "fast-output-before-begin-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: b"FAST-BEFORE-BEGIN".to_vec(),
        }
    );

    drop(pending_launch_guard);
    assert!(matches!(
        decision_rx.await.expect("receive post-Begin cleanup"),
        PendingLaunchDecision::Cleanup {
            terminal_event: PendingLaunchTerminalEvent::Emit,
        }
    ));
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"FAST-BEFORE-BEGIN"
    );
    assert!(rx_event.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn streaming_output_preserves_multibyte_characters_across_chunks() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    stdout_tx
        .send(vec![0xa9])
        .expect("send UTF-8 continuation byte");
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;

    let event = rx_event.recv().await.expect("receive output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: "é".as_bytes().to_vec(),
        }
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        "é".as_bytes()
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn streaming_output_waits_for_begin_when_cancellation_races() -> anyhow::Result<()> {
    let pending_launch = PendingLaunch::new();
    let pending_launch_guard = pending_launch
        .begin()
        .expect("reserved launch should begin exactly once");
    let decision_rx = pending_launch_guard
        .launch()
        .publish()
        .expect("launched process should publish cleanup ownership");
    let harness = streaming_output_harness_with_pending_launch(Arc::clone(&pending_launch)).await?;
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = harness;
    let output_observed = process.output_handles().output_notify.notified();
    tokio::pin!(output_observed);

    stdout_tx
        .send(b"OUTPUT-BEFORE-BEGIN".to_vec())
        .expect("send output before begin");
    (&mut output_observed).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while transcript.lock().await.total_bytes() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("streaming task should capture output");

    assert!(
        rx_event.try_recv().is_err(),
        "output delta must remain hidden until the canonical begin commits"
    );

    let begin_session = Arc::clone(&context.session);
    let begin_turn = Arc::clone(&context.step_context.turn);
    let begin_call_id = context.call_id.clone();
    let cwd = begin_turn
        .environments
        .primary()
        .expect("primary environment")
        .cwd()
        .clone();
    let begin_emitter = ToolEmitter::unified_exec(
        &["proof".to_string()],
        cwd,
        codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
        Some("123".to_string()),
        /*plugin_attribution*/ None,
    );
    let begin_started = Arc::new(tokio::sync::Notify::new());
    let begin_allowed = Arc::new(tokio::sync::Notify::new());
    let begin_started_for_task = Arc::clone(&begin_started);
    let begin_allowed_for_task = Arc::clone(&begin_allowed);
    let begin_rx = pending_launch_guard
        .spawn_manager_owned_begin(async move {
            begin_started_for_task.notify_one();
            begin_allowed_for_task.notified().await;
            begin_emitter
                .emit(
                    ToolEventCtx::new(
                        begin_session.as_ref(),
                        begin_turn.as_ref(),
                        &begin_call_id,
                        /*turn_diff_tracker*/ None,
                    ),
                    ToolEventStage::Begin,
                )
                .await;
        })
        .expect("start event should be admitted");
    begin_started.notified().await;
    drop(pending_launch_guard);
    assert!(
        rx_event.try_recv().is_err(),
        "cancellation during Begin must not release a delta before Begin"
    );
    begin_allowed.notify_one();
    begin_rx
        .await
        .expect("manager-owned start event task should return")
        .expect("start event should commit");

    let item_started = rx_event.recv().await.expect("receive item start");
    let EventMsg::ItemStarted(item_started) = item_started.msg else {
        panic!("expected ItemStarted");
    };
    let TurnItem::CommandExecution(item_started) = item_started.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (item_started.id, item_started.status),
        (
            "streaming-output-test".to_string(),
            CommandExecutionStatus::InProgress,
        )
    );
    let legacy_begin = rx_event.recv().await.expect("receive legacy start");
    let EventMsg::ExecCommandBegin(legacy_begin) = legacy_begin.msg else {
        panic!("expected ExecCommandBegin");
    };
    assert_eq!(legacy_begin.call_id, "streaming-output-test");
    let event = rx_event
        .recv()
        .await
        .expect("receive released output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: b"OUTPUT-BEFORE-BEGIN".to_vec(),
        }
    );

    assert!(matches!(
        decision_rx.await.expect("receive post-Begin cleanup"),
        PendingLaunchDecision::Cleanup {
            terminal_event: PendingLaunchTerminalEvent::Emit,
        }
    ));

    let output_drained_notify = process.output_drained_notify();
    let output_drained = output_drained_notify.notified();
    tokio::pin!(output_drained);
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut output_drained).await;
    assert!(rx_event.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn streaming_output_suppresses_delta_when_launch_is_abandoned_before_begin()
-> anyhow::Result<()> {
    let pending_launch = PendingLaunch::new();
    let pending_launch_guard = pending_launch
        .begin()
        .expect("reserved launch should begin exactly once");
    let decision_rx = pending_launch_guard
        .launch()
        .publish()
        .expect("launched process should publish cleanup ownership");
    let harness = streaming_output_harness_with_pending_launch(Arc::clone(&pending_launch)).await?;
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = harness;
    let output_observed = process.output_handles().output_notify.notified();
    tokio::pin!(output_observed);
    let output_drained_notify = process.output_drained_notify();
    let output_drained = output_drained_notify.notified();
    tokio::pin!(output_drained);

    stdout_tx
        .send(b"ORPHAN-OUTPUT".to_vec())
        .expect("send output before cancellation");
    (&mut output_observed).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while transcript.lock().await.total_bytes() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("streaming task should capture output");
    drop(pending_launch_guard);
    assert!(matches!(
        decision_rx.await.expect("receive cancellation cleanup"),
        PendingLaunchDecision::Cleanup {
            terminal_event: PendingLaunchTerminalEvent::Suppress,
        }
    ));
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut output_drained).await;

    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"ORPHAN-OUTPUT"
    );
    assert!(
        rx_event.try_recv().is_err(),
        "an abandoned pre-Begin launch must not emit an orphan delta"
    );
    Ok(())
}

#[tokio::test]
async fn streaming_output_finishes_on_close_without_waiting_for_grace() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stdout_tx
            .send(b"LATE-OUTPUT-MARKER\xc3".to_vec())
            .expect("send late output");
    });

    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < TRAILING_OUTPUT_GRACE,
        "output close should finish before the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_keeps_grace_as_fallback_without_close() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= TRAILING_OUTPUT_GRACE
            && elapsed <= TRAILING_OUTPUT_GRACE + Duration::from_millis(10),
        "missing output close should use the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        vec![0xc3]
    );
    let event = rx_event.try_recv().expect("receive final output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: vec![0xc3],
        }
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn exit_watcher_waits_for_late_network_denial_before_classifying_end() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = streaming_output_harness().await?;

    tokio::time::pause();
    let process_for_late_denial = Arc::clone(&process);
    let (late_denial_armed_tx, late_denial_armed_rx) = tokio::sync::oneshot::channel();
    let network_denial_launch = PendingLaunch::new();
    let network_denial_monitor = network_denial_launch.spawn_manager_task(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        late_denial_armed_tx.send(()).expect("arm late denial");
        sleep.await;
        process_for_late_denial.fail_and_terminate("LATE_DENIAL".to_string());
    });
    late_denial_armed_rx.await.expect("late denial armed");

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    let _watcher = spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        Some(network_denial_monitor),
        /*plugin_metrics_sidecar*/ None,
    );

    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    drop(stdout_tx);

    let event = rx_event.recv().await.expect("command end event");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (
            item.status,
            item.exit_code,
            item.aggregated_output.as_deref()
        ),
        (
            CommandExecutionStatus::Failed,
            Some(-1),
            Some("LATE_DENIAL")
        )
    );
    assert!(
        elapsed >= Duration::from_millis(10) && elapsed < TRAILING_OUTPUT_GRACE,
        "completion should wait for denial without falling back to the output grace: {elapsed:?}"
    );

    Ok(())
}

#[test]
fn utf8_boundary_preserves_complete_characters() {
    assert_eq!(utf8_boundary(b"hello"), 5);

    let bytes = "ééé".as_bytes();
    assert_eq!(utf8_boundary(&bytes[..3]), 2);

    let bytes = "😀".as_bytes();
    assert_eq!(utf8_boundary(bytes), bytes.len());
    for len in 1..bytes.len() {
        assert_eq!(utf8_boundary(&bytes[..len]), 0);
    }
    assert_eq!(utf8_boundary(&[0xf0, 0x9f, 0x98, 0x80, 0xc3]), 4);
}

#[test]
fn utf8_boundary_batches_malformed_output() {
    assert_eq!(utf8_boundary(&[0xff, b'a', b'b']), 3);
    assert_eq!(utf8_boundary(&[0xff, 0xc3]), 1);
    assert_eq!(utf8_boundary(&[0xff, 0xc3, 0xa9]), 3);
    assert_eq!(utf8_boundary(&[0xe0, 0x80]), 2);

    assert_eq!(utf8_boundary(b"a\xffbbb"), 5);
}

#[tokio::test]
async fn streaming_output_bounds_invalid_bytes_and_keeps_the_full_transcript() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let mut output = Buffer::<8> {
        pending: Vec::new(),
        deferred: HeadTailBuffer::default(),
        event_visibility: PendingLaunchEventVisibility::Emit,
        emitter: Emitter {
            remaining_deltas: 2,
            session,
            turn,
            call_id: "bounded-output-test".to_string(),
            emit_barrier: None,
        },
    };

    // The first frame splits 😀; the last allowed frame leaves é incomplete.
    let bytes = b"\xff\xff\xff\xff\xff\xff\xf0\x9f\x98\x80\xff\xff\xff\xc3\xa9";
    output.push(bytes.to_vec()).await;
    output.push(vec![0xfe, 0xfe]).await;
    output.finish().await;

    let mut chunks = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        chunks.push(delta.chunk);
    }
    assert_eq!(
        chunks,
        vec![
            b"\xff\xff\xff\xff\xff\xff".to_vec(),
            b"\xf0\x9f\x98\x80\xff\xff\xff".to_vec(),
        ]
    );
}

#[tokio::test]
async fn pre_subscription_output_over_cap_replays_bounded_summary_after_begin() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let bytes = vec![b'x'; crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES + 4096];
    let mut buffered_output = HeadTailBuffer::default();
    buffered_output.push_chunk(&bytes);
    let expected = buffered_output.to_bytes_with_omission_marker();
    let mut output = Buffer::<UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES> {
        pending: Vec::new(),
        deferred: HeadTailBuffer::default(),
        event_visibility: PendingLaunchEventVisibility::Pending,
        emitter: Emitter {
            remaining_deltas: crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL,
            session,
            turn,
            call_id: "bounded-pre-subscription-output-test".to_string(),
            emit_barrier: None,
        },
    };

    output.replay_buffered(buffered_output).await;
    assert!(rx_event.try_recv().is_err());
    output
        .set_event_visibility(PendingLaunchEventVisibility::Emit)
        .await;
    output.finish().await;

    let mut emitted = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        assert!(delta.chunk.len() <= UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES);
        emitted.extend(delta.chunk);
    }
    assert_eq!(emitted, expected);
}

#[tokio::test]
async fn deferred_replay_reconciles_more_than_broadcast_capacity_without_loss() -> anyhow::Result<()>
{
    const CHUNKS_DURING_REPLAY: usize = 96;

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let initial = b"before-begin\n".to_vec();
    let mut authoritative = HeadTailBuffer::default();
    authoritative.push_chunk(&initial);
    let transcript = Arc::new(tokio::sync::Mutex::new(authoritative.clone()));
    let emit_entered = Arc::new(tokio::sync::Notify::new());
    let emit_release = Arc::new(tokio::sync::Notify::new());
    let mut output = Buffer::<UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES> {
        pending: Vec::new(),
        deferred: HeadTailBuffer::default(),
        event_visibility: PendingLaunchEventVisibility::Pending,
        emitter: Emitter {
            remaining_deltas: crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL,
            session,
            turn,
            call_id: "replay-lag-test".to_string(),
            emit_barrier: Some(EmitBarrier {
                entered: Arc::clone(&emit_entered),
                release: Arc::clone(&emit_release),
            }),
        },
    };
    output.replay_buffered(authoritative).await;

    let mut observed_bytes = initial.len();
    let transcript_for_replay = Arc::clone(&transcript);
    let (last_chunk_tx, last_chunk_rx) = tokio::sync::oneshot::channel();
    let replay = tokio::spawn(async move {
        output
            .set_event_visibility(PendingLaunchEventVisibility::Emit)
            .await;
        let last_chunk = last_chunk_rx.await.expect("publish last lagged chunk");
        output
            .push_published(last_chunk, &transcript_for_replay, &mut observed_bytes)
            .await;
        output.finish().await;
    });

    emit_entered.notified().await;
    let mut expected = initial;
    let mut last_chunk = None;
    {
        let mut authoritative = transcript.lock().await;
        for index in 0..CHUNKS_DURING_REPLAY {
            let bytes = format!("during-replay-{index:03}\n").into_bytes();
            let offset = authoritative.total_bytes();
            authoritative.push_chunk(&bytes);
            expected.extend_from_slice(&bytes);
            last_chunk = Some(PublishedOutputChunk { offset, bytes });
        }
    }
    last_chunk_tx
        .send(last_chunk.expect("at least one chunk should be published"))
        .expect("replay task should await the last chunk");
    emit_release.notify_one();
    replay.await?;

    let mut emitted = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        emitted.extend(delta.chunk);
    }
    assert_eq!(emitted, expected);
    Ok(())
}
