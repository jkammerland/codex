use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use codex_utils_absolute_path::AbsolutePathBuf;

use crate::AppServerTarget;
use crate::app_server_connection::IncompatibleLocalDaemon;
use codex_app_server_client::RemoteAppServerEndpoint;

const AUTO_CONNECT_DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DefaultDaemonSocket {
    Absent,
    Compatible(AbsolutePathBuf),
    Incompatible {
        socket_path: AbsolutePathBuf,
        reported_version: Option<String>,
    },
    Unavailable {
        socket_path: AbsolutePathBuf,
        reason: String,
    },
}

impl DefaultDaemonSocket {
    pub(crate) fn into_compatible_path(self) -> Option<AbsolutePathBuf> {
        match self {
            Self::Compatible(socket_path) => Some(socket_path),
            Self::Absent | Self::Incompatible { .. } | Self::Unavailable { .. } => None,
        }
    }

    pub(crate) fn blocks_embedded_mutation(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    pub(crate) fn shared_agents_error(&self) -> Option<String> {
        match self {
            Self::Absent | Self::Compatible(_) => None,
            Self::Incompatible {
                socket_path,
                reported_version,
            } => Some(format!(
                "the shared app-server daemon at {} reports version {}; finish daemon-backed jobs and restart it with this Codex runtime",
                socket_path.display(),
                reported_version.as_deref().unwrap_or("unknown"),
            )),
            Self::Unavailable {
                socket_path,
                reason,
            } => Some(format!(
                "the shared app-server daemon at {} could not be verified: {reason}",
                socket_path.display(),
            )),
        }
    }
}

pub(crate) async fn probe_default_daemon_socket(codex_home: &Path) -> DefaultDaemonSocket {
    let Ok(socket_path) = codex_app_server_client::app_server_control_socket_path(codex_home)
    else {
        return DefaultDaemonSocket::Absent;
    };

    #[cfg(windows)]
    let (probe_path, _directory_guard) =
        match codex_uds::validate_private_socket_path(socket_path.as_path()) {
            Ok((path, guard)) => (path, guard),
            Err(error) => {
                return DefaultDaemonSocket::Unavailable {
                    socket_path,
                    reason: error.to_string(),
                };
            }
        };
    #[cfg(not(windows))]
    let probe_path = socket_path.as_path();

    let probe_stream = match tokio::time::timeout(
        AUTO_CONNECT_DAEMON_CONNECT_TIMEOUT,
        codex_uds::UnixStream::connect(probe_path),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if error.kind() == ErrorKind::NotFound => {
            return DefaultDaemonSocket::Absent;
        }
        Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
            return match codex_uds::is_stale_socket_path(socket_path.as_path()).await {
                Ok(true) => DefaultDaemonSocket::Absent,
                Ok(false) => DefaultDaemonSocket::Unavailable {
                    socket_path,
                    reason: "the control path is not a stale socket".to_string(),
                },
                Err(stale_error) => DefaultDaemonSocket::Unavailable {
                    socket_path,
                    reason: stale_error.to_string(),
                },
            };
        }
        Ok(Err(error)) => {
            return DefaultDaemonSocket::Unavailable {
                socket_path,
                reason: error.to_string(),
            };
        }
        Err(_) => {
            return DefaultDaemonSocket::Unavailable {
                socket_path,
                reason: format!(
                    "connection did not complete within {} ms",
                    AUTO_CONNECT_DAEMON_CONNECT_TIMEOUT.as_millis()
                ),
            };
        }
    };
    #[cfg(windows)]
    if let Err(error) = probe_stream.ensure_non_elevated_peer() {
        return DefaultDaemonSocket::Unavailable {
            socket_path,
            reason: error.to_string(),
        };
    }
    drop(probe_stream);

    let target = AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        },
        allow_embedded_fallback: false,
    };
    match crate::app_server_connection::connect(&target).await {
        Ok(app_server) => {
            let _ = app_server.shutdown().await;
            DefaultDaemonSocket::Compatible(socket_path)
        }
        Err(error) => {
            if let Some(incompatible) = error.downcast_ref::<IncompatibleLocalDaemon>() {
                return DefaultDaemonSocket::Incompatible {
                    socket_path,
                    reported_version: incompatible.reported_version().map(str::to_string),
                };
            }
            DefaultDaemonSocket::Unavailable {
                socket_path,
                reason: error.to_string(),
            }
        }
    }
}

#[cfg(all(test, unix))]
#[path = "daemon_socket_tests.rs"]
mod tests;
