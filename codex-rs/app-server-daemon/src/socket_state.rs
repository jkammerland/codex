use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

pub(crate) async fn is_confirmed_absent(socket_path: &Path) -> Result<bool> {
    match codex_uds::UnixStream::connect(socket_path).await {
        Ok(_stream) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
            codex_uds::is_stale_socket_path(socket_path)
                .await
                .with_context(|| {
                    format!(
                        "failed to inspect unresponsive app-server socket {}",
                        socket_path.display()
                    )
                })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to verify app-server socket state at {}",
                socket_path.display()
            )
        }),
    }
}

#[cfg(all(test, unix))]
#[path = "socket_state_tests.rs"]
mod tests;
