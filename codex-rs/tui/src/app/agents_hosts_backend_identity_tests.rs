//! Backend provenance survives host transfers while transport epochs reject stale intake.

use super::*;
use crate::app_server_session::AppServerSessionEvent;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn host_attachment_round_trip_preserves_backend_identity() -> Result<()> {
    use std::os::fd::FromRawFd;
    use std::process::Command;
    use std::process::Stdio;

    // A successful handoff drains terminal input. Run the actual transition in a PTY child,
    // isolating crossterm's process-global reader from nextest's redirected standard input.
    if std::env::var_os("CODEX_HOST_IDENTITY_PTY").is_none() {
        let mut master = -1;
        let mut slave = -1;
        // Safety: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
        );
        // Safety: ownership of the newly allocated descriptors transfers to these files.
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "app::agents_hosts::tests::backend_identity::host_attachment_round_trip_preserves_backend_identity",
                "--nocapture",
            ])
            .env("CODEX_HOST_IDENTITY_PTY", "1")
            .stdin(Stdio::from(slave))
            .output()?;
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "PTY child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return Ok(());
    }

    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut foreground = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let mut other = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let owners = [foreground.backend_id(), other.backend_id()];
    assert_ne!(owners[0], owners[1]);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let alpha = key("alpha");
    let beta = key("beta");
    let alpha_started = foreground.start_thread(&app.config).await?;
    let beta_started = other.start_thread(&app.config).await?;
    let alpha_bootstrap = foreground.bootstrap(&app.config).await?;
    let beta_bootstrap = other.bootstrap(&app.config).await?;
    app.agents_hosts.manifest = Some(PathBuf::from("manifest.toml"));
    app.agents_hosts.foreground = alpha.clone();
    app.agents_hosts.hosts.insert(alpha.clone(), host("Alpha"));
    let mut beta_host = host("Beta");
    beta_host.session = Some(other);
    app.agents_hosts.hosts.insert(beta.clone(), beta_host);

    for (generation, target, owner, started, bootstrap) in [
        (1, beta, owners[1], beta_started, beta_bootstrap),
        (2, alpha, owners[0], alpha_started, alpha_bootstrap),
    ] {
        app.agents_hosts.prepare_generation = generation;
        let request = app.agents_hosts.hosts[&target]
            .session
            .as_ref()
            .expect("parked host owns its session")
            .request_copy();
        let scope = ConnectionScope {
            host: target.clone(),
            epoch: 1,
        };
        let config = app.config.clone();
        app.commit_agents_host(
            &mut tui,
            &mut foreground,
            scope,
            generation,
            Ok(Box::new(PreparedHost {
                config,
                bootstrap,
                started,
                read_only: false,
            })),
            request,
        )
        .await?;
        assert_eq!(
            (
                app.agents_hosts.foreground.clone(),
                foreground.backend_id(),
                foreground.request_copy().backend_id(),
            ),
            (target, owner, owner),
        );
    }
    foreground.shutdown().await?;
    for parked in app.agents_hosts.hosts.values_mut() {
        if let Some(session) = parked.session.take() {
            session.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn host_intake_rejects_foreign_owner_and_stale_epoch_before_metadata_changes() -> Result<()> {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let mut foreground = AppServerSession::discovery_placeholder();
    let parked = AppServerSession::discovery_placeholder();
    let parked_owner = parked.backend_id();
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let machine = key("parked");
    let mut entry = host("Parked");
    entry.session = Some(parked);
    entry.epoch = 2;
    entry.refreshing = true;
    app.agents_hosts.hosts.insert(machine.clone(), entry);
    let id = ThreadId::new();

    for (backend_id, epoch) in [(foreground.backend_id(), 2), (parked_owner, 1)] {
        app.handle_agents_host_update(
            &mut tui,
            &mut foreground,
            HostUpdate::Event {
                scope: ConnectionScope {
                    host: machine.clone(),
                    epoch,
                },
                event: AppServerSessionEvent {
                    backend_id,
                    event: AppServerEvent::ServerNotification(Box::new(
                        ServerNotification::ThreadNameUpdated(ThreadNameUpdatedNotification {
                            thread_id: id.to_string(),
                            thread_name: Some("Stale title".into()),
                        }),
                    )),
                },
            },
        )
        .await?;
    }
    let entry = &app.agents_hosts.hosts[&machine];
    assert_eq!(entry.revision, 0);
    assert!(entry.refresh_metadata.is_empty());
    assert!(entry.state.agents_overview.threads.is_empty());
    assert!(entry.buffered.is_empty());
    Ok(())
}

#[tokio::test]
async fn host_eof_preserves_pending_startup_before_and_after_rejected_admission() -> Result<()> {
    for rejected in [false, true] {
        let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
        let mut session = AppServerSession::discovery_placeholder();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let machine = key("starting");
        app.agents_hosts.foreground = machine.clone();
        app.agents_hosts
            .hosts
            .insert(machine.clone(), host("Starting"));
        app.pending_startup_thread_start = true;
        app.app_server_target = AppServerTarget::Remote {
            endpoint: crate::resolve_remote_addr("ws://127.0.0.1:1")?,
        };
        app.chat_widget.initial_user_message = Some("startup prompt".into());
        let backends = crate::app::backend_sessions::BackendSessions::new(session.request_copy());
        let slot = backends
            .slot(session.backend_id())
            .expect("foreground slot");
        assert!(app.startup_connection_is_draining(&backends, slot));
        if rejected {
            assert!(app.begin_reconnect_for_rejected_startup());
        }
        app.handle_agents_host_update(
            &mut tui,
            &mut session,
            HostUpdate::Offline(ConnectionScope {
                host: machine.clone(),
                epoch: 1,
            }),
        )
        .await?;
        assert!(app.pending_startup_thread_start);
        assert_eq!(app.reconnect.offline, rejected);
        assert!(!app.agents_hosts.hosts[&machine].connected);
        assert!(!app.startup_connection_is_draining(&backends, slot));
        assert_eq!(
            app.chat_widget
                .initial_user_message
                .as_ref()
                .map(|message| message.text.as_str()),
            Some("startup prompt"),
        );
    }
    Ok(())
}
