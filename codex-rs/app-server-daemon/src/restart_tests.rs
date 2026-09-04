#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::Result;
use pretty_assertions::assert_eq;
#[cfg(unix)]
use tempfile::TempDir;

use super::RestartDecision;
use super::RestartIfRunningOutcome;
use super::RestartMode;
use super::UpdaterRefreshMode;
use super::restart_decision;
use super::should_reexec_updater;
#[cfg(unix)]
use crate::Daemon;
#[cfg(unix)]
use crate::backend;
use crate::client::ProbeInfo;
#[cfg(unix)]
use crate::settings::DaemonSettings;

#[cfg(unix)]
fn test_daemon(temp_dir: &TempDir, managed_codex_bin: PathBuf) -> Daemon {
    Daemon {
        socket_path: temp_dir.path().join("app-server-control.sock"),
        pid_file: temp_dir.path().join("app-server.pid"),
        update_pid_file: temp_dir.path().join("app-server-updater.pid"),
        operation_lock_file: temp_dir.path().join("daemon.lock"),
        settings_file: temp_dir.path().join("settings.json"),
        managed_codex_bin,
    }
}

#[test]
fn updater_reexec_waits_for_validated_restart() {
    assert_eq!(
        [
            RestartIfRunningOutcome::Busy,
            RestartIfRunningOutcome::NotReady,
            RestartIfRunningOutcome::AlreadyCurrent,
            RestartIfRunningOutcome::NotRunning,
            RestartIfRunningOutcome::Restarted,
        ]
        .map(|outcome| {
            should_reexec_updater(UpdaterRefreshMode::ReexecIfManagedBinaryChanged, outcome)
        }),
        [false, false, false, false, true]
    );
}

#[test]
fn unchanged_updater_never_reexecs() {
    assert_eq!(
        [
            RestartIfRunningOutcome::Busy,
            RestartIfRunningOutcome::NotReady,
            RestartIfRunningOutcome::AlreadyCurrent,
            RestartIfRunningOutcome::NotRunning,
            RestartIfRunningOutcome::Restarted,
        ]
        .map(|outcome| should_reexec_updater(UpdaterRefreshMode::None, outcome)),
        [false, false, false, false, false]
    );
}

#[test]
fn restart_decision_preserves_forced_refreshes() {
    let current_info = ProbeInfo {
        app_server_version: "0.1.0".to_string(),
    };

    assert_eq!(
        [
            restart_decision(
                RestartMode::IfVersionChanged,
                Some(&current_info),
                Some("0.1.0"),
            ),
            restart_decision(
                RestartMode::IfVersionChanged,
                /*info*/ None,
                /*managed_version*/ None,
            ),
            restart_decision(RestartMode::Always, Some(&current_info), Some("0.1.0")),
            restart_decision(
                RestartMode::Always,
                /*info*/ None,
                /*managed_version*/ None,
            ),
        ],
        [
            RestartDecision::AlreadyCurrent,
            RestartDecision::NotReady,
            RestartDecision::Restart,
            RestartDecision::Restart,
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn incompatible_managed_version_is_rejected_before_live_daemon_is_stopped() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let managed_codex_bin = temp_dir.path().join("codex-test");
    tokio::fs::write(
        &managed_codex_bin,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'codex incompatible-test-version\\n'\n  exit 0\nfi\nexec sleep 60\n",
    )
    .await?;
    let mut permissions = tokio::fs::metadata(&managed_codex_bin).await?.permissions();
    permissions.set_mode(0o700);
    tokio::fs::set_permissions(&managed_codex_bin, permissions).await?;
    let daemon = test_daemon(&temp_dir, managed_codex_bin.clone());
    let settings = DaemonSettings::default();
    let backend = backend::pid_backend(daemon.backend_paths(&settings));
    backend.start().await?.expect("detached test daemon pid");

    let restart_result = daemon
        .try_restart_if_running(
            RestartMode::Always,
            UpdaterRefreshMode::None,
            &managed_codex_bin,
        )
        .await;
    let remained_running = backend.is_starting_or_running().await;
    let cleanup_result = backend.stop().await;
    let remained_running = remained_running?;
    cleanup_result?;

    assert!(remained_running, "validation stopped the live daemon");
    assert_eq!(
        restart_result.expect_err("incompatible managed binary must be rejected").to_string(),
        format!(
            "managed Codex executable {} reports version incompatible-test-version, but this runtime is {}\n\n\
             The running daemon was left untouched. Install the matching maintained fork before retrying.",
            managed_codex_bin.display(),
            codex_build_info::CODEX_CLI_VERSION,
        )
    );
    Ok(())
}
