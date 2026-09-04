//! Applies the selected server's socket policy on initial connections and reconnects.

use crate::AppServerTarget;
#[cfg(windows)]
use crate::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use crate::connect_remote_app_server;
use crate::version::CODEX_CLI_VERSION;
use codex_app_server_client::AppServerClient;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerClient;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerConnectArgs;
#[cfg(windows)]
use codex_app_server_client::RemoteAppServerEndpoint;
#[cfg(windows)]
use codex_utils_absolute_path::AbsolutePathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
#[error(
    "local app-server daemon version {reported_version} does not match this Codex runtime {expected_version}\n\n\
     The existing daemon was left running so active work is not interrupted. Finish daemon-backed jobs, then restart a managed daemon with:\n  \
     codex app-server daemon restart\n\
     If the listener is unmanaged, stop the process that owns the local app-server socket instead."
)]
pub(crate) struct IncompatibleLocalDaemon {
    reported_version: String,
    expected_version: &'static str,
}

impl IncompatibleLocalDaemon {
    pub(crate) fn reported_version(&self) -> Option<&str> {
        (self.reported_version != "unknown").then_some(self.reported_version.as_str())
    }
}

pub(crate) async fn connect(target: &AppServerTarget) -> color_eyre::Result<AppServerClient> {
    let app_server = match target {
        AppServerTarget::Embedded => {
            color_eyre::eyre::bail!("embedded sessions have no remote connection")
        }
        #[cfg(windows)]
        AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        } => {
            // Revalidate at the real connection, not just the earlier discovery probe.
            let (socket_path, _directory) =
                codex_uds::validate_private_socket_path(socket_path.as_path())?;
            let app_server =
                RemoteAppServerClient::connect_local_daemon(RemoteAppServerConnectArgs {
                    endpoint: RemoteAppServerEndpoint::UnixSocket {
                        socket_path: AbsolutePathBuf::from_absolute_path_checked(socket_path)?,
                    },
                    client_name: "codex-tui".to_string(),
                    client_version: CODEX_CLI_VERSION.to_string(),
                    experimental_api: true,
                    mcp_server_openai_form_elicitation: false,
                    opt_out_notification_methods: Vec::new(),
                    channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
                })
                .await?;
            AppServerClient::Remote(app_server)
        }
        AppServerTarget::LocalDaemon { endpoint } | AppServerTarget::Remote { endpoint } => {
            connect_remote_app_server(endpoint.clone()).await?
        }
    };

    if matches!(target, AppServerTarget::LocalDaemon { .. }) {
        let reported_version = match &app_server {
            AppServerClient::Remote(client) => client.server_version(),
            AppServerClient::InProcess(_) => None,
        };
        if reported_version != Some(CODEX_CLI_VERSION) {
            let error = IncompatibleLocalDaemon {
                reported_version: reported_version.unwrap_or("unknown").to_string(),
                expected_version: CODEX_CLI_VERSION,
            };
            let _ = app_server.shutdown().await;
            return Err(error.into());
        }
    }

    Ok(app_server)
}
