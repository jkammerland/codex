use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_protocol::*;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::model::server_key;
use crate::transport::ResolvedEndpoint;

pub(crate) fn start(coordinator: &Arc<Coordinator>) -> tokio::task::JoinSet<()> {
    let mut tasks = tokio::task::JoinSet::new();
    let publisher = coordinator.clone();
    let mut changed = coordinator.changed.subscribe();
    tasks.spawn(async move {
        loop {
            if let Err(error) = publisher.publish().await {
                tracing::error!(%error, "could not publish team messages");
            }
            if changed.changed().await.is_err() {
                break;
            }
        }
    });
    for machine in &coordinator.manifest.machines {
        for server in &machine.servers {
            tasks.spawn(run(coordinator.clone(), machine.clone(), server.clone()));
        }
    }
    tasks
}

async fn run(coordinator: Arc<Coordinator>, machine: Machine, server: Server) {
    let key = server_key(&machine.id, &server.id);
    let mut delay = 1;
    loop {
        let result = connect(&coordinator, &machine, &server).await;
        coordinator.peers.write().await.remove(&key);
        if let Some(status) = coordinator.statuses.write().await.get_mut(&key) {
            if status.connected {
                delay = 1;
            }
            status.connected = false;
            status.error = Some(
                result
                    .err()
                    .map_or_else(|| "Disconnected".into(), |error| error.to_string()),
            );
        }
        coordinator.notify();
        // Only transport recovery uses a timer. Idle agents and durable mail
        // are driven by notifications, never by repeated model turns.
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}

async fn connect(coordinator: &Arc<Coordinator>, machine: &Machine, server: &Server) -> Result<()> {
    let endpoint = ResolvedEndpoint::open(&server.endpoint).await?;
    let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: endpoint.endpoint.clone(),
        client_name: "codex-team".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 1024,
    })
    .await?;
    let peer = Peer {
        handle: AppServerRequestHandle::Remote(client.request_handle()),
        endpoint,
    };
    let key = server_key(&machine.id, &server.id);
    let mut changed = coordinator.changed.subscribe();
    coordinator.reconcile(machine, server, &peer).await?;
    coordinator
        .peers
        .write()
        .await
        .insert(key.clone(), peer.clone());
    coordinator.notify();
    loop {
        tokio::select! {
            result = changed.changed() => {
                result?;
                coordinator.synchronize(machine, server, &peer).await?;
            }
            event = client.next_event() => match event {
                Some(AppServerEvent::ServerNotification(notification)) => match *notification {
                    ServerNotification::TeamChanged(_) => coordinator.synchronize(machine, server, &peer).await?,
                    notification => coordinator.observe(machine, server, notification).await?,
                },
                // Approval requests stay with the owning human connection.
                // An operator relay has an independent connection and can answer.
                Some(AppServerEvent::ServerRequest(_)) => {},
                Some(AppServerEvent::Lagged { .. }) => bail!("worker notification stream lagged; reconciling durable state"),
                Some(AppServerEvent::Disconnected { message }) => bail!("{message}"),
                None => bail!("worker connection closed"),
            },
        }
    }
}
