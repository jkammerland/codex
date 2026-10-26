//! Keeps ordered transport intake independent of foreground RPCs and widget ownership.

use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerPath;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

type ShutdownReply = oneshot::Sender<std::io::Result<()>>;
type ShutdownRequest = oneshot::Sender<ShutdownReply>;

pub(super) struct Connection {
    requests: Option<AppServerRequestHandle>,
    pub(super) events: Option<mpsc::UnboundedReceiver<AppServerEvent>>,
    shutdown: std::sync::Arc<std::sync::Mutex<Option<ShutdownRequest>>>,
    worker: Option<std::sync::Arc<IntakeWorker>>,
    family: Option<String>,
    os: Option<String>,
    version: Option<String>,
    home: Option<AppServerPath>,
    remote_home: Option<String>,
    embedded: bool,
}

impl Connection {
    /// An empty overview owns no server or account until a configured host is attached.
    pub(super) fn discovery_placeholder() -> Self {
        Self {
            requests: None,
            events: None,
            shutdown: std::sync::Arc::new(std::sync::Mutex::new(None)),
            worker: None,
            family: None,
            os: None,
            version: None,
            home: None,
            remote_home: None,
            embedded: false,
        }
    }

    pub(super) fn is_discovery_placeholder(&self) -> bool {
        self.requests.is_none()
    }

    pub(super) fn try_request_handle(&self) -> Option<AppServerRequestHandle> {
        self.requests.clone()
    }

    pub(super) fn new(mut client: AppServerClient) -> Self {
        let embedded = matches!(client, AppServerClient::InProcess(_));
        let requests = client.request_handle();
        let family = client.platform_family().map(str::to_owned);
        let os = client.platform_os().map(str::to_owned);
        let (version, remote_home) = match &client {
            AppServerClient::Remote(remote) => (
                remote.server_version().map(str::to_owned),
                remote.codex_home().map(str::to_owned),
            ),
            AppServerClient::Cloud(_) | AppServerClient::InProcess(_) => (None, None),
        };
        let home = remote_home.clone().map(AppServerPath::from_app_server);
        let (sender, events) = mpsc::unbounded_channel();
        let (shutdown, mut stop) = oneshot::channel::<ShutdownReply>();
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = &mut stop => {
                        if let Ok(reply) = result {
                            let _ = reply.send(client.shutdown().await);
                        }
                        break;
                    }
                    event = client.next_event() => {
                        let Some(event) = event else { break };
                        if sender.send(event).is_err() { break; }
                    }
                }
            }
        });
        Self {
            requests: Some(requests),
            events: Some(events),
            shutdown: std::sync::Arc::new(std::sync::Mutex::new(Some(shutdown))),
            worker: Some(std::sync::Arc::new(IntakeWorker(worker))),
            family,
            os,
            version,
            home,
            remote_home,
            embedded,
        }
    }

    pub(super) fn request_copy(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            events: None,
            shutdown: self.shutdown.clone(),
            worker: self.worker.clone(),
            family: self.family.clone(),
            os: self.os.clone(),
            version: self.version.clone(),
            home: self.home.clone(),
            remote_home: self.remote_home.clone(),
            embedded: self.embedded,
        }
    }

    pub(super) fn is_embedded(&self) -> bool {
        self.embedded
    }

    pub(super) fn platform_family(&self) -> Option<&str> {
        self.family.as_deref()
    }
    pub(super) fn platform_os(&self) -> Option<&str> {
        self.os.as_deref()
    }
    pub(super) fn server_version(&self) -> Option<&str> {
        self.version.as_deref()
    }
    pub(super) fn server_codex_home(&self) -> Option<&str> {
        self.remote_home.as_deref()
    }
    pub(super) fn codex_home(&self, local: &AbsolutePathBuf) -> Option<AppServerPath> {
        self.home.clone().or_else(|| {
            self.embedded
                .then(|| AppServerPath::from_app_server(local.display().to_string()))
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "This API requires a connected session; discovery placeholders use try_request_handle."
    )]
    pub(super) fn request_handle(&self) -> AppServerRequestHandle {
        self.requests
            .clone()
            .expect("a connected session owns request transport")
    }

    pub(super) async fn request_typed<T: DeserializeOwned>(
        &self,
        request: ClientRequest,
    ) -> Result<T, TypedRequestError> {
        let Some(requests) = self.requests.as_ref() else {
            return Err(TypedRequestError::Transport {
                method: "agents/discovery".into(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "Choose a configured machine first",
                ),
            });
        };
        requests.request_typed(request).await
    }

    pub(super) async fn resolve_server_request(
        &self,
        id: RequestId,
        result: codex_app_server_protocol::Result,
    ) -> std::io::Result<()> {
        self.requests
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotConnected, "No machine is attached")
            })?
            .server_request_reply_handle(id)
            .resolve(result)
            .await
    }

    pub(super) async fn next_event(&mut self) -> Option<AppServerEvent> {
        match self.events.as_mut() {
            Some(events) => events.recv().await,
            None => std::future::pending().await,
        }
    }

    pub(super) async fn shutdown(self) -> std::io::Result<()> {
        let (reply, result) = oneshot::channel();
        let shutdown = self
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if shutdown.is_some_and(|stop| stop.send(reply).is_ok()) {
            result.await.unwrap_or(Ok(()))
        } else {
            Ok(())
        }
    }
}

struct IntakeWorker(tokio::task::JoinHandle<()>);

impl Drop for IntakeWorker {
    fn drop(&mut self) {
        // UI exit closes its transport; remote turns and owned jobs remain server-owned.
        self.0.abort();
    }
}
