//! Prepares server state off the terminal loop, then commits one foreground transition.

use super::agents_hosts::ConnectionScope;
use super::agents_hosts::HostBufferedEvent;
use super::agents_hosts::HostState;
use super::agents_hosts::HostUpdate;
use super::agents_hosts::PreparedHost;
use super::agents_hosts::ThreadTarget;
use super::*;

impl App {
    pub(super) fn prepare_agents_host(
        &mut self,
        target: ThreadTarget,
        foreground: &mut AppServerSession,
    ) {
        if self.reject_pending_permission_root_switch() {
            return;
        }
        let is_foreground = target.host == self.agents_hosts.foreground;
        if is_foreground {
            self.agents_hosts.route(
                self.agents_hosts.scope(),
                foreground,
                self.app_event_tx.unscoped(),
            );
        }
        let Some(host) = self.agents_hosts.hosts.get_mut(&target.host) else {
            return;
        };
        let Some(mut session) = (if is_foreground {
            Some(foreground.request_copy())
        } else {
            host.session.as_ref().map(AppServerSession::request_copy)
        }) else {
            self.agents_hosts.notice = Some("This machine is connecting".into());
            return;
        };
        self.agents_hosts.prepare_generation += 1;
        let generation = self.agents_hosts.prepare_generation;
        let scope = ConnectionScope {
            host: target.host,
            epoch: host.epoch,
        };
        let config = host
            .state
            .config
            .clone()
            .unwrap_or_else(|| self.config.clone());
        let settings = host
            .state
            .local_settings
            .clone()
            .unwrap_or_else(|| self.local_settings.clone());
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.notice = Some("Preparing task…".into());
        self.agents_hosts.spawn(async move {
            let result = async {
                let bootstrap = session.bootstrap(&config).await?;
                let (started, read_only) = match session
                    .resume_thread(
                        &settings,
                        config.clone(),
                        target.thread_id,
                        crate::app_server_session::ResumeModelSettings::PreserveExistingThread,
                    )
                    .await
                {
                    Ok(started) => (started, false),
                    Err(error) if crate::app_server_session::is_active_writer_error(&error) => {
                        let (started, _) = session
                            .read_thread_for_viewing(&config, &settings, target.thread_id)
                            .await?;
                        (started, true)
                    }
                    Err(error) => return Err(error),
                };
                Ok::<_, color_eyre::Report>(Box::new(PreparedHost {
                    config,
                    bootstrap,
                    started,
                    read_only,
                }))
            }
            .await
            .map_err(|_| {
                "Could not attach to this task; the current conversation is still available"
                    .to_string()
            });
            sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Prepared {
                scope,
                generation,
                result,
                session,
            })));
        });
    }

    pub(super) fn start_other_host_task(
        &mut self,
        host_key: super::agents_hosts::HostKey,
        cwd: Option<codex_utils_path_uri::LegacyAppPathString>,
        fork_parent: Option<ThreadId>,
        foreground: &mut AppServerSession,
    ) {
        let is_foreground = host_key == self.agents_hosts.foreground;
        if is_foreground {
            self.agents_hosts.route(
                self.agents_hosts.scope(),
                foreground,
                self.app_event_tx.unscoped(),
            );
        }
        let Some(host) = self.agents_hosts.hosts.get_mut(&host_key) else {
            return;
        };
        let Some(mut session) = (if is_foreground {
            Some(foreground.request_copy())
        } else {
            host.session.as_ref().map(AppServerSession::request_copy)
        }) else {
            return;
        };
        self.agents_hosts.prepare_generation += 1;
        let generation = self.agents_hosts.prepare_generation;
        let scope = ConnectionScope {
            host: host_key,
            epoch: host.epoch,
        };
        let mut config = host
            .state
            .config
            .clone()
            .unwrap_or_else(|| self.config.clone());
        let settings = host
            .state
            .local_settings
            .clone()
            .unwrap_or_else(|| self.local_settings.clone());
        if !host.target.uses_remote_workspace()
            && let Some(cwd) = &cwd
            && let Some(cwd) = cwd.to_inferred_abs_path()
        {
            config.cwd = cwd;
        }
        config.model = None;
        config.model_reasoning_effort = None;
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.spawn(async move {
            let result = async {
                let bootstrap = session.bootstrap_for_new_thread(&config).await?;
                let started = if let Some(parent) = fork_parent {
                    session
                        .fork_thread_with_host_defaults(
                            &settings,
                            &config,
                            parent,
                            cwd.as_ref().map(|cwd| std::path::Path::new(cwd.as_str())),
                        )
                        .await?
                } else {
                    session
                        .start_thread_with_host_defaults(
                            &settings,
                            &config,
                            cwd.as_ref().map(|cwd| std::path::Path::new(cwd.as_str())),
                        )
                        .await?
                };
                Ok::<_, color_eyre::Report>(Box::new(PreparedHost {
                    config,
                    bootstrap,
                    started,
                    read_only: false,
                }))
            }
            .await
            .map_err(|_| "Could not create a task on this machine".into());
            sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::Prepared {
                scope,
                generation,
                result,
                session,
            })));
        });
    }

    pub(super) async fn commit_agents_host(
        &mut self,
        tui: &mut tui::Tui,
        foreground: &mut AppServerSession,
        scope: ConnectionScope,
        generation: u64,
        result: Result<Box<PreparedHost>, String>,
        session: AppServerSession,
    ) -> Result<()> {
        if !self.agents_hosts.accepts(&scope) {
            return Ok(());
        }
        if generation != self.agents_hosts.prepare_generation {
            return Ok(());
        }
        let mut prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.agents_hosts.notice = Some(error);
                self.repaint_agents_overview();
                return Ok(());
            }
        };
        if !self.agents_hosts.hosts[&scope.host]
            .target
            .uses_remote_workspace()
        {
            crate::executor_paths::sync_local_config_paths(
                &mut prepared.config,
                &prepared.started.session.cwd,
                &prepared.started.session.runtime_workspace_roots,
            )
            .map_err(color_eyre::eyre::Report::msg)?;
        }
        if let Some(id) = self.current_displayed_thread_id()
            && let Some(input) = self.chat_widget.capture_thread_input_state()
        {
            self.agents_overview.input_states.insert(id, input);
        }
        self.store_active_thread_receiver().await;
        // All preparation succeeded. Transfer the host-owned state in one foreground step.
        let source = self.agents_hosts.foreground.clone();
        let overview_view_state = Arc::clone(&self.agents_overview.view_state);
        let mut target_state = HostState::default();
        let buffered;
        if source != scope.host {
            let mut source_state = HostState::default();
            source_state.swap(self);
            #[expect(
                clippy::expect_used,
                reason = "The target scope was validated before committing; state swaps do not remove host entries."
            )]
            let target_host = self
                .agents_hosts
                .hosts
                .get_mut(&scope.host)
                .expect("host exists");
            target_state = std::mem::take(&mut target_host.state);
            buffered = std::mem::take(&mut target_host.buffered);
            target_host.session = None;
            target_state.swap(self);
            let mut old_session = std::mem::replace(foreground, session);
            let source_scope = self.agents_hosts.scope();
            self.agents_hosts
                .route(source_scope, &mut old_session, self.app_event_tx.unscoped());
            #[expect(
                clippy::expect_used,
                reason = "The foreground source was registered when host pooling began and is retained during attachment."
            )]
            let source_host = self
                .agents_hosts
                .hosts
                .get_mut(&source)
                .expect("source exists");
            source_host.state = source_state;
            source_host.session = Some(old_session);
            if !source_host.connected
                && !source_host
                    .session
                    .as_ref()
                    .is_some_and(AppServerSession::is_discovery_placeholder)
            {
                self.connect_agents_host(source.clone());
            }
        } else {
            // The request copy shares intake ownership; replacing it cannot close this transport.
            *foreground = session;
            buffered = VecDeque::new();
        }
        // reset_thread_event_state retains pending approvals for every subscribed host thread.
        // Seed this after restoring either same-host or cross-host state.
        for id in self.thread_event_channels.keys() {
            self.agents_overview
                .dispatched_requests
                .entry(*id)
                .or_default();
        }
        self.agents_overview.view_state = overview_view_state;
        self.agents_hosts.foreground = scope.host.clone();
        self.agents_hosts.generation += 1;
        let generation = self.agents_hosts.generation;
        self.app_server_target = self.agents_hosts.hosts[&scope.host].target.clone();
        self.config = prepared.config;
        self.config.model_provider_id = prepared.started.session.model_provider_id.clone();
        self.config.model = Some(prepared.started.session.model.clone());
        self.config.model_reasoning_effort = prepared.started.session.reasoning_effort.clone();
        self.local_settings = self.local_settings.reloaded(&self.config);
        self.model_catalog = Arc::new(
            ModelCatalog::new(prepared.bootstrap.available_models)
                .with_collaboration_modes(prepared.bootstrap.collaboration_modes),
        );
        self.reconnect = Default::default();
        self.app_event_tx = self
            .app_event_tx
            .unscoped()
            .scoped(scope.clone(), generation);
        self.workspace_command_runner = Some(Arc::new(AppServerWorkspaceCommandRunner::new(
            foreground.request_handle(),
        )));
        // Replacement inherits connection metadata before it replays the target session. The old
        // host's status must not make a remote POSIX cwd look like a local filesystem path.
        self.chat_widget.remote_connection =
            crate::status::remote_connection::remote_connection_status_value(
                &self.app_server_target,
                foreground.server_version(),
            );
        let attached_thread_id = prepared.started.session.thread_id;
        self.replace_chat_widget_with_app_server_thread(
            tui,
            prepared.started,
            session_lifecycle::ThreadAttachPresentation::SessionLineage,
            /*initial_user_message*/ None,
        )
        .await?;
        self.chat_widget.requires_openai_auth = prepared.bootstrap.requires_openai_auth;
        self.chat_widget.update_account_state(
            prepared.bootstrap.status_account_display,
            prepared.bootstrap.plan_type,
            prepared.bootstrap.has_chatgpt_account,
            matches!(
                prepared.bootstrap.auth_mode,
                Some(TelemetryAuthMode::Chatgpt)
            ),
        );
        self.feedback_audience = prepared.bootstrap.feedback_audience;
        self.chat_widget.snapshot_local_images = self.app_server_target.uses_remote_workspace();
        self.chat_widget.remote_connection =
            crate::status::remote_connection::remote_connection_status_value(
                &self.app_server_target,
                foreground.server_version(),
            );
        if prepared.read_only {
            self.ensure_thread_channel(attached_thread_id)
                .mark_external_writer();
            self.chat_widget.show_external_writer_thread();
        }
        for event in buffered {
            match event {
                HostBufferedEvent::Event(event) => {
                    self.handle_app_server_event(foreground, event).await
                }
                HostBufferedEvent::Request(request) => {
                    self.handle_server_request_event(foreground, *request).await
                }
            }
        }
        if let Some(error) = self
            .pending_host_prompt_reverts
            .get_mut(&attached_thread_id)
            .and_then(|pending| pending.error.as_mut())
            && error.reload_required
        {
            error.reload_required = false;
            error.message =
                "Prompt edit could not be confirmed; this task's history has been reloaded.".into();
        }
        self.complete_host_prompt_revert(attached_thread_id).await?;
        for event in target_state.pending_app_events.drain(..) {
            self.app_event_tx.unscoped().send(event);
        }
        if let Some(id) = self.primary_thread_id
            && let Some(input) = self.agents_overview.input_states.remove(&id)
        {
            self.chat_widget.restore_thread_input_state(
                Some(input),
                crate::chatwidget::ThreadInputStateRestoreMode {
                    preserve_in_flight_turn: self.active_turn_id_for_thread(id).await.is_some(),
                },
            );
        }
        self.agents_hosts.notice = None;
        self.cancel_pending_key_chord();
        self.pending_right_click_paste = None;
        self.startup_protected_input_boundary = true;
        self.discard_startup_modal_input(tui)?;

        Ok(())
    }
}
