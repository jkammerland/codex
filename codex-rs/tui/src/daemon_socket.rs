use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

use codex_utils_absolute_path::AbsolutePathBuf;

use crate::version::CODEX_CLI_VERSION;

#[cfg(unix)]
const AUTO_CONNECT_DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(unix)]
pub(crate) async fn maybe_probe_default_daemon_socket(
    codex_home: &Path,
) -> Option<AbsolutePathBuf> {
    let socket_path = codex_app_server_client::app_server_control_socket_path(codex_home).ok()?;
    match tokio::time::timeout(
        AUTO_CONNECT_DAEMON_PROBE_TIMEOUT,
        codex_app_server_daemon::probe_app_server_version(socket_path.as_path()),
    )
    .await
    {
        Ok(Ok(app_server_version)) if app_server_version == CODEX_CLI_VERSION => Some(socket_path),
        Ok(Ok(app_server_version)) => {
            tracing::warn!(
                %app_server_version,
                expected_version = CODEX_CLI_VERSION,
                socket_path = %socket_path.display(),
                "skipping incompatible default app-server daemon socket"
            );
            None
        }
        Ok(Err(err)) => {
            tracing::debug!(%err, socket_path = %socket_path.display(), "skipping default app-server daemon socket");
            None
        }
        Err(_) => {
            tracing::debug!(
                socket_path = %socket_path.display(),
                timeout_ms = AUTO_CONNECT_DAEMON_PROBE_TIMEOUT.as_millis(),
                "timed out probing default app-server daemon socket"
            );
            None
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn maybe_probe_default_daemon_socket(
    _codex_home: &Path,
) -> Option<AbsolutePathBuf> {
    None
}

#[cfg(all(test, unix))]
#[path = "daemon_socket_tests.rs"]
mod tests;
