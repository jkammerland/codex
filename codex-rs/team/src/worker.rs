use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
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
    // Administrative RPCs have their own owner so a slow enrollment never
    // blocks ordinary mail publication or priority execution controls.
    let administrator = coordinator.clone();
    let mut administration_changed = coordinator.changed.subscribe();
    tasks.spawn(async move {
        loop {
            administration_changed.borrow_and_update();
            if let Err(error) = administrator.advance_administration().await {
                tracing::error!(%error, "team administration requires reconciliation");
            }
            if administration_changed.changed().await.is_err() {
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
        let result = async {
            let endpoint = ResolvedEndpoint::open(&server.endpoint).await?;
            coordinator
                .endpoints
                .write()
                .await
                .insert(key.clone(), endpoint.clone());
            coordinator.notify();
            connect(&coordinator, &machine, &server, endpoint).await
        }
        .await;
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

async fn connect(
    coordinator: &Arc<Coordinator>,
    machine: &Machine,
    server: &Server,
    endpoint: ResolvedEndpoint,
) -> Result<()> {
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
        verified: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let key = server_key(&machine.id, &server.id);
    let mut changed = coordinator.changed.subscribe();
    let (dirty, mut pending) = tokio::sync::watch::channel(0_u64);
    let sync_coordinator = coordinator.clone();
    let sync_machine = machine.clone();
    let sync_server = server.clone();
    let sync_peer = peer.clone();
    let sync_key = key.clone();
    let synchronize = async move {
        let coordinator = sync_coordinator;
        let machine = sync_machine;
        let server = sync_server;
        let peer = sync_peer;
        coordinator.reconcile(&machine, &server, &peer).await?;
        coordinator
            .peers
            .write()
            .await
            .insert(sync_key, peer.clone());
        coordinator.notify();
        loop {
            pending.changed().await?;
            pending.borrow_and_update();
            coordinator.synchronize(&machine, &server, &peer).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let control_coordinator = coordinator.clone();
    let control_machine = machine.clone();
    let control_server = server.clone();
    let control_dispatch = async move {
        let coordinator = control_coordinator;
        let mut changed = coordinator.changed.subscribe();
        loop {
            changed.borrow_and_update();
            coordinator
                .dispatch_controls(&control_machine, &control_server, &peer)
                .await?;
            changed.changed().await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let mut work = spawn_connection_work(synchronize, control_dispatch);
    loop {
        tokio::select! {
            result = work.join_next() => return result.context("worker background task ended")?.context("worker background task failed")?,
            result = changed.changed() => {
                result?;
                dirty.send_modify(|revision| *revision += 1);
            }
            event = client.next_event() => match event {
                Some(AppServerEvent::ServerNotification(notification)) => {
                    observe_notification(coordinator, machine, server, *notification, &dirty).await?;
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

async fn observe_notification(
    coordinator: &Coordinator,
    machine: &Machine,
    server: &Server,
    notification: ServerNotification,
    dirty: &tokio::sync::watch::Sender<u64>,
) -> Result<()> {
    // Changes to executor facts, selections and approval flags can leave the
    // legacy status/model strings unchanged. Refresh through the existing lane.
    let refresh = matches!(
        &notification,
        ServerNotification::TeamChanged(_)
            | ServerNotification::EnvironmentConnected(_)
            | ServerNotification::EnvironmentDisconnected(_)
            | ServerNotification::ThreadSettingsUpdated(_)
            | ServerNotification::ThreadStatusChanged(_)
    );
    if matches!(
        &notification,
        ServerNotification::TeamChanged(_)
            | ServerNotification::EnvironmentConnected(_)
            | ServerNotification::EnvironmentDisconnected(_)
    ) {
        observe_team_changed(coordinator, &server_key(&machine.id, &server.id)).await;
    } else {
        coordinator.observe(machine, server, notification).await?;
    }
    if refresh {
        dirty.send_modify(|revision| *revision += 1);
    }
    Ok(())
}

fn spawn_connection_work(
    synchronize: impl Future<Output = Result<()>> + Send + 'static,
    control_dispatch: impl Future<Output = Result<()>> + Send + 'static,
) -> tokio::task::JoinSet<Result<()>> {
    // Notification intake may wait for a mutation owned by synchronization.
    // Separate task owners keep that owner and priority controls progressing.
    let mut work = tokio::task::JoinSet::new();
    work.spawn(synchronize);
    work.spawn(control_dispatch);
    work
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "notification version changes share the mutation fence used when committing host snapshots"
)]
async fn observe_team_changed(coordinator: &Coordinator, key: &str) {
    let _mutation = coordinator.mutations.lock().await;
    *coordinator
        .observed_versions
        .lock()
        .await
        .entry(key.to_owned())
        .or_default() += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the regression deliberately holds a mutation guard while proving the notification and control task owners progress independently"
    )]
    async fn notification_wait_does_not_suspend_mutation_owner_or_priority_controls() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
        let (owned, owner_ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let (control, requested) = tokio::sync::oneshot::channel();
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        let synchronizer = coordinator.clone();
        let mut work = spawn_connection_work(
            async move {
                let _mutation = synchronizer.mutations.lock().await;
                owned
                    .send(())
                    .map_err(|_| anyhow::anyhow!("notification owner disappeared"))?;
                released.await?;
                Ok(())
            },
            async move {
                requested.await?;
                acknowledge
                    .send(())
                    .map_err(|_| anyhow::anyhow!("control owner disappeared"))?;
                std::future::pending::<Result<()>>().await
            },
        );
        owner_ready.await?;

        // Intake has already reached the lock held by the synchronizer before
        // either the independent control or the synchronizer is released.
        let notification = observe_team_changed(&coordinator, "worker/primary");
        tokio::pin!(notification);
        assert!(futures::poll!(&mut notification).is_pending());
        control
            .send(())
            .map_err(|_| anyhow::anyhow!("control lane ended"))?;
        acknowledged.await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("synchronizer ended"))?;
        notification.await;
        work.join_next()
            .await
            .context("synchronizer task missing")???;
        assert_eq!(
            coordinator
                .observed_versions
                .lock()
                .await
                .get("worker/primary"),
            Some(&1)
        );
        work.abort_all();
        while work.join_next().await.is_some() {}
        Ok(())
    }
}

#[cfg(test)]
#[path = "worker_discovery_tests.rs"]
mod discovery_tests;
