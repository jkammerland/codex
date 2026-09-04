//! Coordinates daemon replacement so validation finishes before process mutation.

use std::path::Path;

use anyhow::Result;
use anyhow::anyhow;

use crate::Daemon;
use crate::client;
use crate::managed_install::managed_codex_version;
use crate::runtime_identity;
use crate::try_lock_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartIfRunningOutcome {
    Busy,
    NotRunning,
    NotReady,
    AlreadyCurrent,
    Restarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartMode {
    IfVersionChanged,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdaterRefreshMode {
    None,
    ReexecIfManagedBinaryChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartDecision {
    NotReady,
    AlreadyCurrent,
    Restart,
}

impl Daemon {
    pub(crate) async fn try_restart_if_running(
        &self,
        mode: RestartMode,
        updater_refresh_mode: UpdaterRefreshMode,
        managed_codex_bin: &Path,
    ) -> Result<RestartIfRunningOutcome> {
        let operation_lock = self.open_operation_lock_file().await?;
        if !try_lock_file(&operation_lock)? {
            return Ok(RestartIfRunningOutcome::Busy);
        }
        let settings = self.load_settings().await?;
        let outcome = if let Some(backend) = self.running_backend_instance(&settings).await? {
            let info = client::probe(&self.socket_path).await.ok();
            let managed_version = managed_codex_version(managed_codex_bin).await?;
            runtime_identity::validate_managed_version(managed_codex_bin, &managed_version)?;
            match restart_decision(mode, info.as_ref(), Some(&managed_version)) {
                RestartDecision::NotReady => return Ok(RestartIfRunningOutcome::NotReady),
                RestartDecision::AlreadyCurrent => RestartIfRunningOutcome::AlreadyCurrent,
                RestartDecision::Restart => {
                    #[cfg(windows)]
                    crate::backend::windows::ensure_detached_launch(managed_codex_bin)?;
                    backend.stop().await?;
                    let _ = self
                        .start_managed_backend_and_validate(&settings, managed_codex_bin)
                        .await?;
                    RestartIfRunningOutcome::Restarted
                }
            }
        } else if client::probe(&self.socket_path).await.is_ok() {
            return Err(anyhow!(
                "app server is running but is not managed by codex app-server daemon"
            ));
        } else {
            RestartIfRunningOutcome::NotRunning
        };

        #[cfg(unix)]
        if should_reexec_updater(updater_refresh_mode, outcome) {
            crate::update_loop::reexec_managed_updater(managed_codex_bin)?;
        }
        #[cfg(windows)]
        if should_reexec_updater(updater_refresh_mode, outcome) {
            crate::backend::pid_update_loop_backend(
                self.backend_paths_with_bin(&settings, managed_codex_bin),
            )
            .replace_current_updater()
            .await?;
        }

        Ok(outcome)
    }
}

fn restart_decision(
    mode: RestartMode,
    info: Option<&client::ProbeInfo>,
    managed_version: Option<&str>,
) -> RestartDecision {
    match (mode, info, managed_version) {
        (RestartMode::IfVersionChanged, None, _) => RestartDecision::NotReady,
        (RestartMode::IfVersionChanged, Some(info), Some(managed_version))
            if info.app_server_version == managed_version =>
        {
            RestartDecision::AlreadyCurrent
        }
        _ => RestartDecision::Restart,
    }
}

fn should_reexec_updater(
    updater_refresh_mode: UpdaterRefreshMode,
    outcome: RestartIfRunningOutcome,
) -> bool {
    updater_refresh_mode == UpdaterRefreshMode::ReexecIfManagedBinaryChanged
        && outcome == RestartIfRunningOutcome::Restarted
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
