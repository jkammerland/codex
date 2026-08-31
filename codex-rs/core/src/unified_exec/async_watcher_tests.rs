use std::sync::Arc;

use super::Buffer;
use super::Emitter;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
#[cfg(unix)]
use codex_exec_server::Environment;
#[cfg(unix)]
use codex_exec_server::ExecParams;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;
#[cfg(unix)]
use codex_utils_path_uri::PathUri;

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
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    start_streaming_output(&process, &context, Arc::clone(&transcript));

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
async fn streaming_output_drains_late_output_after_exit_before_close() -> anyhow::Result<()> {
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
        tokio::time::sleep(Duration::from_millis(150)).await;
        stdout_tx
            .send(b"LATE-OUTPUT-MARKER\xc3".to_vec())
            .expect("send late output");
    });

    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(150),
        "output close should wait for the late output: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_bounds_a_permanently_inherited_descriptor() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    } = streaming_output_harness().await?;
    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id,
        vec!["descriptor-proof".to_string()],
        cwd,
        /*process_id*/ 124,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        /*network_denial_monitor*/ None,
        /*plugin_metrics_sidecar*/ None,
    );

    tokio::task::yield_now().await;
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::time::sleep(
        super::super::process_manager::POST_EXIT_CLOSE_WAIT_CAP - Duration::from_millis(25),
    )
    .await;
    assert!(
        rx_event.try_recv().is_err(),
        "streaming output must preserve the full post-exit grace"
    );
    let event = tokio::time::timeout(Duration::from_secs(2), rx_event.recv())
        .await
        .expect("exit watcher should finish after its grace")
        .expect("command end event");
    assert!(
        exited_at.elapsed() >= super::super::process_manager::POST_EXIT_CLOSE_WAIT_CAP,
        "output collection ended before the post-exit grace"
    );
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(item.status, CommandExecutionStatus::Completed);
    drop(stdout_tx);

    Ok(())
}

#[tokio::test]
async fn streaming_output_drains_a_chunk_queued_at_the_exit_cutoff() -> anyhow::Result<()> {
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
    exit_tx.send(0).expect("send exit");
    process.cancellation_token().cancelled().await;
    tokio::time::advance(Duration::from_millis(1)).await;
    process.publish_output_for_test(b"QUEUED-AT-CUTOFF".to_vec());
    tokio::time::advance(super::super::process_manager::POST_EXIT_CLOSE_WAIT_CAP).await;
    (&mut drained).await;
    tokio::time::resume();

    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"QUEUED-AT-CUTOFF"
    );
    drop(stdout_tx);

    Ok(())
}

#[tokio::test]
async fn streaming_output_recovers_the_terminal_transcript_after_receiver_lag() -> anyhow::Result<()>
{
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
    let mut expected = Vec::new();
    {
        let mut authoritative_transcript = process.output_handles().output_buffer.lock().await;
        for index in 0..100u8 {
            let chunk = vec![index];
            expected.extend_from_slice(&chunk);
            authoritative_transcript.push_chunk(&chunk);
        }
    }
    for index in 0..100u8 {
        let chunk = vec![index];
        process.publish_output_for_test(chunk);
    }
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;

    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        expected
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn exec_server_root_exit_bounds_an_inherited_output_descriptor() -> anyhow::Result<()> {
    let environment = Environment::create_for_tests(/*exec_server_url*/ None)?;
    let backend = environment.get_exec_backend();
    let started = backend
        .start(ExecParams {
            process_id: "inherited-descriptor-proof".to_string().into(),
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "(sleep 0.1; printf LATE-OUTPUT) & exit 0".to_string(),
            ],
            cwd: PathUri::from_host_native_path(std::env::current_dir()?)?,
            shell_snapshot: None,
            env_policy: /*env_policy*/ None,
            env: Default::default(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
            network_proxy: None,
        })
        .await?;
    let process = Arc::new(UnifiedExecProcess::from_exec_server_started(started).await?);
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "exec-server-descriptor-test".to_string(),
    );
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    start_streaming_output(&process, &context, Arc::clone(&transcript));
    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    let started_at = Instant::now();
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        context.call_id,
        vec!["exec-server-descriptor-proof".to_string()],
        cwd,
        /*process_id*/ 125,
        /*plugin_attribution*/ None,
        transcript,
        started_at,
        /*network_denial_monitor*/ None,
        /*plugin_metrics_sidecar*/ None,
    );

    let completed = tokio::time::timeout(Duration::from_millis(750), async {
        loop {
            let event = rx_event.recv().await.expect("command event");
            if let EventMsg::ItemCompleted(completed) = event.msg {
                return completed;
            }
        }
    })
    .await
    .expect("root exit should not wait for the inherited descriptor");
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(item.status, CommandExecutionStatus::Completed);
    assert_eq!(item.aggregated_output.as_deref(), Some("LATE-OUTPUT"));
    assert!(
        started_at.elapsed() < Duration::from_millis(750),
        "completion should use the bounded post-exit grace"
    );

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
    let network_denial_monitor = tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        late_denial_armed_tx.send(()).expect("arm late denial");
        sleep.await;
        process_for_late_denial.fail_and_terminate("LATE_DENIAL".to_string());
    });
    late_denial_armed_rx.await.expect("late denial armed");

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    spawn_exit_watcher(
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
        elapsed >= Duration::from_millis(10),
        "completion should wait for the late denial: {elapsed:?}"
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
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let mut output = Buffer::<8> {
        pending: Vec::new(),
        transcript: Arc::clone(&transcript),
        emitter: Emitter {
            remaining_deltas: 2,
            session,
            turn,
            call_id: "bounded-output-test".to_string(),
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

    let mut expected_transcript = bytes.to_vec();
    expected_transcript.extend([0xfe, 0xfe]);
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        expected_transcript
    );
}
