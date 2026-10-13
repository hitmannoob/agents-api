use std::sync::Arc;

use super::Buffer;
use super::Emitter;
use super::TRAILING_OUTPUT_GRACE;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::OutputBuffers;
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
    output_receiver: tokio::sync::broadcast::Receiver<Vec<u8>>,
    stdout_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_tx: tokio::sync::oneshot::Sender<i32>,
    output_buffer: Arc<tokio::sync::Mutex<OutputBuffers>>,
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
    let mut process =
        UnifiedExecProcess::from_spawned(spawned, SandboxType::None, Box::new(NoopSpawnLifecycle))
            .await?;
    let output_receiver = process.take_output_receiver()?;
    let process = Arc::new(process);
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "streaming-output-test".to_string(),
    );
    let output_buffer = Arc::clone(&process.output_handles().output_buffer);

    Ok(StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        context,
        rx_event,
    })
}

#[test_case::test_case(b""; "no_late_output")]
#[test_case::test_case(b"late\n"; "with_late_output")]
#[tokio::test]
async fn completed_output_preserves_bytes_before_watcher(late_output: &[u8]) -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        context,
        rx_event,
    } = streaming_output_harness().await?;

    let collected = process.output_handles().output_notify.notified();
    tokio::pin!(collected);
    collected.as_mut().enable();
    stdout_tx.send(b"early\n".to_vec())?;
    collected.await;

    let model_output = UnifiedExecProcessManager::collect_output_until_deadline(
        process.output_handles(),
        /*pause_state*/ None,
        Instant::now(),
    )
    .await;
    assert_eq!(model_output.to_bytes(), b"early\n");

    start_streaming_output(&process, &context, output_receiver);
    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    spawn_exit_watcher(
        Arc::clone(&process),
        &context,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        output_buffer,
        Instant::now(),
        /*network_denial_monitor*/ None,
        /*plugin_metrics_sidecar*/ None,
    );
    stdout_tx.send(late_output.to_vec())?;
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");

    let mut deltas = Vec::new();
    let item = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx_event.recv().await?.msg {
                EventMsg::ExecCommandOutputDelta(delta) => deltas.extend(delta.chunk),
                EventMsg::ItemCompleted(completed) => {
                    if let TurnItem::CommandExecution(item) = completed.item {
                        return Ok::<_, async_channel::RecvError>(item);
                    }
                }
                _ => {}
            }
        }
    })
    .await??;
    let expected_output = String::from_utf8([b"early\n".as_slice(), late_output].concat())?;
    assert_eq!(deltas, expected_output.as_bytes());
    assert_eq!(
        (item.status, item.exit_code, item.aggregated_output),
        (
            CommandExecutionStatus::Completed,
            Some(0),
            Some(expected_output)
        )
    );
    Ok(())
}

#[cfg(unix)]
#[test_case::test_case("printf 'early\\n'", b"early\n"; "early_exit")]
#[test_case::test_case("printf 'early\\n'; sleep 0.3; printf 'late\\n'", b"early\nlate\n"; "later_output")]
#[tokio::test]
async fn exec_server_output_preserves_bytes_before_watcher(
    command: &str,
    expected_output: &[u8],
) -> anyhow::Result<()> {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "exec-server-output-test".to_string(),
    );
    let environment = codex_exec_server::Environment::default_for_tests();
    let started = environment
        .get_exec_backend()
        .start(codex_exec_server::ExecParams {
            metadata: None,
            process_id: "exec-server-output-test".to_string().into(),
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            cwd,
            shell_snapshot: None,
            env_policy: None,
            env: std::env::vars().collect(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
            network_proxy: None,
        })
        .await?;
    let mut process = UnifiedExecProcess::from_exec_server_started(started).await?;
    let output_receiver = process.take_output_receiver()?;
    start_streaming_output(&process, &context, output_receiver);
    tokio::time::timeout(
        Duration::from_secs(2),
        process.output_drained_notify().notified(),
    )
    .await?;

    let mut deltas = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        assert_eq!(delta.call_id, context.call_id);
        deltas.extend(delta.chunk);
    }
    assert_eq!(deltas, expected_output);
    Ok(())
}

#[tokio::test]
async fn streaming_output_drains_lagged_receiver() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        context,
        rx_event,
        ..
    } = streaming_output_harness().await?;

    for index in 0..65 {
        let collected = process.output_handles().output_notify.notified();
        tokio::pin!(collected);
        collected.as_mut().enable();
        stdout_tx.send(format!("{index}\n").into_bytes())?;
        collected.await;
    }
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    start_streaming_output(&process, &context, output_receiver);
    tokio::time::timeout(
        Duration::from_secs(2),
        process.output_drained_notify().notified(),
    )
    .await?;

    let mut deltas = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        deltas.extend(delta.chunk);
    }
    let expected_output = (1..65)
        .map(|index| format!("{index}\n"))
        .collect::<String>();
    assert_eq!(deltas, expected_output.as_bytes());
    Ok(())
}

#[tokio::test]
async fn streaming_output_preserves_multibyte_characters_across_chunks() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        rx_event,
        context,
    } = streaming_output_harness().await?;
    start_streaming_output(&process, &context, output_receiver);
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
        output_buffer
            .lock()
            .await
            .transcript
            .to_bytes_with_omission_marker(),
        "é".as_bytes()
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn streaming_output_finishes_on_close_without_waiting_for_grace() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        context,
        ..
    } = streaming_output_harness().await?;
    start_streaming_output(&process, &context, output_receiver);
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
        output_buffer
            .lock()
            .await
            .transcript
            .to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_keeps_grace_as_fallback_without_close() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        rx_event,
        context,
    } = streaming_output_harness().await?;
    start_streaming_output(&process, &context, output_receiver);
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
        output_buffer
            .lock()
            .await
            .transcript
            .to_bytes_with_omission_marker(),
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
        output_receiver,
        stdout_tx,
        exit_tx,
        output_buffer,
        mut context,
        rx_event,
    } = streaming_output_harness().await?;
    start_streaming_output(&process, &context, output_receiver);

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
    let step = Arc::get_mut(&mut context.step_context).expect("unshared test step");
    let model_info = Arc::make_mut(&mut Arc::make_mut(&mut step.settings).model_info);
    model_info.truncation_policy = codex_protocol::openai_models::TruncationPolicyConfig {
        mode: codex_protocol::openai_models::TruncationMode::Bytes,
        limit: 4,
    };
    spawn_exit_watcher(
        Arc::clone(&process),
        &context,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        output_buffer,
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
async fn streaming_output_bounds_invalid_bytes() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let mut output = Buffer::<8> {
        pending: Vec::new(),
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
}
