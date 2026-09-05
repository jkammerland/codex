use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CodeModeSessionProvider;
use pretty_assertions::assert_eq;

use super::ProcessOwnedCodeModeSession;
use super::ProcessOwnedCodeModeSessionProvider;
use super::connection::ConnectionError;
use crate::NoopCodeModeSessionDelegate;
use crate::generation_cell_id::stale_cell_outcome;

#[test]
fn provider_reuses_its_live_process_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::default();

    let first = provider.process_host();
    let second = provider.process_host();

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn missing_host_error_limits_the_displayed_path_to_512_bytes() {
    let executable = "codex-code-mode-host-does-not-exist";
    let host_program = format!("{}{executable}", "missing-directory/".repeat(/*n*/ 64));
    let expected_suffix = &host_program[host_program.len() - (512 - "...".len())..];
    let error = ConnectionError::Spawn {
        host_program: PathBuf::from(&host_program),
        error: io::Error::new(io::ErrorKind::NotFound, "host unavailable"),
    };

    assert_eq!(
        error.to_string(),
        format!("failed to spawn code-mode host ...{expected_suffix}: host unavailable")
    );
}

#[test]
fn missing_host_error_preserves_utf8_boundaries_when_truncating_the_path() {
    let executable = "codex-code-mode-host-does-not-exist";
    let host_program = format!("{}{executable}", "🦀".repeat(/*n*/ 256));
    let error = ConnectionError::Spawn {
        host_program: PathBuf::from(host_program),
        error: io::Error::new(io::ErrorKind::NotFound, "host unavailable"),
    }
    .to_string();
    let displayed_path = error
        .strip_prefix("failed to spawn code-mode host ")
        .and_then(|message| message.strip_suffix(": host unavailable"))
        .expect("missing-host error should contain the displayed host path");

    assert!(displayed_path.starts_with("..."));
    assert!(displayed_path.ends_with(executable));
    assert!(displayed_path.len() <= 512);
}

#[test]
fn stale_generation_cell_becomes_a_terminal_failed_outcome() {
    let cell_id = codex_code_mode_protocol::CellId::new("first-generation-cell".to_string());
    let current_cell_id = crate::generation_cell_id::encode_public_cell_id(
        /*generation*/ 2,
        "current-generation-cell",
    );

    assert_eq!(
        stale_cell_outcome(/*generation*/ 2, &cell_id),
        Some(codex_code_mode_protocol::WaitOutcome::MissingCell(
            codex_code_mode_protocol::RuntimeResponse::Result {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
                error_text: Some("cell belongs to a stale code-mode host generation".to_string(),),
                code_mode_host_duration: None,
            }
        ))
    );
    assert_eq!(stale_cell_outcome(/*generation*/ 1, &cell_id), None);
    assert_eq!(stale_cell_outcome(/*generation*/ 2, &current_cell_id), None);
}

#[test]
fn resumed_process_session_cannot_alias_a_reused_host_cell_id() {
    let previous = ProcessOwnedCodeModeSession::new();
    let resumed = ProcessOwnedCodeModeSession::new();
    let previous_generation = previous.inner.next_generation.load(Ordering::Relaxed);
    let resumed_generation = resumed.inner.next_generation.load(Ordering::Relaxed);
    assert!(previous_generation > 1);
    assert!(resumed_generation > 1);
    assert_ne!(previous_generation, resumed_generation);

    let previous_handle =
        crate::generation_cell_id::encode_public_cell_id(previous_generation, "reused-host-cell");
    let resumed_handle =
        crate::generation_cell_id::encode_public_cell_id(resumed_generation, "reused-host-cell");

    assert_ne!(previous_handle, resumed_handle);
    assert!(stale_cell_outcome(resumed_generation, &previous_handle).is_some());
    assert_eq!(
        stale_cell_outcome(resumed_generation, &resumed_handle),
        None
    );
    assert!(
        stale_cell_outcome(
            resumed_generation,
            &codex_code_mode_protocol::CellId::new("reused-host-cell".to_string()),
        )
        .is_some(),
        "legacy raw handles must be terminal after rollout resume"
    );
}

#[tokio::test]
async fn provider_returns_missing_host_error() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        "codex-code-mode-host-does-not-exist".into(),
    );

    let error = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .err()
        .expect("missing host should fail");

    assert!(error.contains("failed to spawn code-mode host codex-code-mode-host-does-not-exist"));
}

#[tokio::test]
async fn shutdown_before_open_does_not_spawn_the_host() {
    let session = ProcessOwnedCodeModeSession::new();

    session.shutdown().await.expect("shutdown session");
    let error = session
        .execute(codex_code_mode_protocol::ExecuteRequest {
            tool_call_id: "call-1".to_string(),
            enabled_tools: Vec::new(),
            source: "text('unreachable')".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .err()
        .expect("shutdown session should reject execution");

    assert_eq!(error, "code mode session is shutting down");
}
