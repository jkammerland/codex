//! Owns curated Git commands without waiting for unrelated plugin refreshes.

use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

#[derive(Default)]
pub(crate) struct CuratedGitLifecycle {
    registration: Mutex<()>,
    cancelled: CancellationToken,
    commands: TaskTracker,
}

impl CuratedGitLifecycle {
    pub(crate) fn ensure_running(&self) -> Result<(), String> {
        if self.cancelled.is_cancelled() {
            Err("curated plugin sync cancelled during shutdown".to_string())
        } else {
            Ok(())
        }
    }

    fn register(&self) -> Result<TaskTrackerToken, String> {
        let _registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ensure_running()?;
        Ok(self.commands.token())
    }

    pub(crate) fn publish(
        &self,
        publish: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // Suppress work observed after cancellation, but do not join filesystem
        // cleanup or callbacks already underway. Only Git commands own drain tokens.
        self.ensure_running()?;
        publish()
    }

    pub(crate) fn cancel(&self) {
        // Closing and registration share this gate: shutdown cannot miss a command.
        let _registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancelled.cancel();
        self.commands.close();
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel();
        self.commands.wait().await;
    }

    pub(crate) fn run_git(
        &self,
        command: Command,
        context: &str,
        timeout: Duration,
    ) -> Result<Output, String> {
        let _command = self.register()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to create curated Git runtime: {err}"))?;
        let mut command = tokio::process::Command::from(command);
        runtime
            .block_on(
                codex_git_utils::run_git_command_with_timeout_and_cancellation_output(
                    &mut command,
                    timeout,
                    self.cancelled.cancelled(),
                ),
            )
            .map_err(|err| format!("failed to run {context}: {err}"))
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
