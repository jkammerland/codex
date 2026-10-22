//! Team creation reuses normal startup with a durable, worker-reserved identity.

use super::*;
use codex_app_server_protocol::TeamMemberCreateParams;
use codex_app_server_protocol::TeamMemberCreatePhase;
use codex_app_server_protocol::TeamMemberCreateResponse;
use codex_models_manager::manager::RefreshStrategy;
use codex_team_extension::MemberCreateIntent;
use codex_team_extension::TeamHostService;

pub(super) enum ThreadStartupTarget {
    Client,
    Member(Box<MemberStartup>),
}

pub(super) struct MemberStartup {
    intent: MemberCreateIntent,
    service: Arc<TeamHostService>,
    auth_manager: Arc<AuthManager>,
}

impl ThreadStartupTarget {
    pub(super) async fn validate_config(&self, config: &Config) -> Result<(), JSONRPCErrorError> {
        let Self::Member(member) = self else {
            return Ok(());
        };
        if config.ephemeral {
            return Err(invalid_request(
                "team member creation requires a durable session",
            ));
        }
        let manager = codex_core::build_models_manager(config, Arc::clone(&member.auth_manager));
        let catalog = manager
            .list_models(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await;
        let model = manager
            .get_default_model(
                &config.model,
                /*allow_provider_model_fallback*/ false,
                RefreshStrategy::Offline,
                config.http_client_factory(),
            )
            .await;
        // Preserve the existing uncatalogued custom-provider policy, without
        // accepting a missing first-party or explicitly advertised model.
        if let Some(preset) = catalog.iter().find(|preset| preset.model == model) {
            if let Some(effort) = &config.model_reasoning_effort
                && (member.intent.params.effort.is_some()
                    || !preset.supported_reasoning_efforts.is_empty())
                && !preset
                    .supported_reasoning_efforts
                    .iter()
                    .any(|supported| &supported.effort == effort)
            {
                return Err(invalid_request(format!(
                    "reasoning effort {effort} is not supported by {model}"
                )));
            }
        } else if config.model_provider_id == codex_model_provider_info::OPENAI_PROVIDER_ID
            || config.model_catalog.is_some()
            || config.model_provider.model_catalog_url.is_some()
        {
            return Err(invalid_request(format!(
                "model {model} is not available on this worker"
            )));
        }
        Ok(())
    }

    pub(super) fn reserved_thread_id(&self) -> Result<Option<ThreadId>, JSONRPCErrorError> {
        match self {
            Self::Client => Ok(None),
            Self::Member(member) => ThreadId::from_string(&member.intent.thread_id)
                .map(Some)
                .map_err(|error| invalid_request(error.to_string())),
        }
    }

    pub(super) async fn mark_starting(&self) -> Result<(), JSONRPCErrorError> {
        if let Self::Member(member) = self {
            member
                .service
                .mark_member_creation_starting(&member.intent.params)
                .await
                .map_err(|error| internal_error(error.to_string()))?;
        }
        Ok(())
    }
}

impl ThreadRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn team_member_create(
        &self,
        request_id: ConnectionRequestId,
        params: TeamMemberCreateParams,
        service: Arc<TeamHostService>,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        client_mcp_extensions: ClientMcpExtensions,
        request_context: RequestContext,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let context = self.listener_task_context();
        let outgoing = Arc::clone(&self.outgoing);
        let config_manager = self.config_manager.clone();
        let thread_store = Arc::clone(&self.thread_store);
        let auth_manager = Arc::clone(&self.auth_manager);
        let warnings = Arc::clone(&self.initial_config_warnings);
        let trace = request_context.request_trace();
        let error_request_id = request_id.clone();
        let task = async move {
            let result = Self::team_member_create_task(
                context,
                thread_store,
                config_manager,
                request_id.clone(),
                params,
                service,
                auth_manager,
                app_server_client_name,
                app_server_client_version,
                client_mcp_extensions,
                trace,
                warnings,
            )
            .await;
            match result {
                Ok(response) => outgoing.send_response(request_id, response).await,
                Err(error) => outgoing.send_error(error_request_id, error).await,
            }
        };
        self.background_tasks
            .spawn(task.instrument(request_context.span()))
            .await
            .map_err(|_| internal_error("team member creation stopped before completing"))?;
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn team_member_create_task(
        context: ListenerTaskContext,
        thread_store: Arc<dyn ThreadStore>,
        config_manager: ConfigManager,
        request_id: ConnectionRequestId,
        params: TeamMemberCreateParams,
        service: Arc<TeamHostService>,
        auth_manager: Arc<AuthManager>,
        client_name: Option<String>,
        client_version: Option<String>,
        client_mcp_extensions: ClientMcpExtensions,
        trace: Option<W3cTraceContext>,
        warnings: Arc<Vec<ConfigWarningNotification>>,
    ) -> Result<TeamMemberCreateResponse, JSONRPCErrorError> {
        let _creation = service
            .lock_member_creation(&params)
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
        let intent = service
            .prepare_member_creation(params.clone())
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
        if let Some(outcome) = &intent.outcome
            && outcome.phase == TeamMemberCreatePhase::Failed
        {
            return Ok(outcome.clone());
        }
        let thread_id = ThreadId::from_string(&intent.thread_id)
            .map_err(|error| internal_error(error.to_string()))?;

        // Recover this reservation only. Never guess using cwd, aliases, or the newest thread.
        if intent.starting {
            let recovered =
                recover_member_root(&context.thread_manager, thread_store.as_ref(), thread_id)
                    .await;
            let (phase, error) = match recovered {
                Ok(true) => (TeamMemberCreatePhase::Created, None),
                Ok(false) => (TeamMemberCreatePhase::NeedsInspection, Some("startup may have begun, but the reserved root has no durable history; inspect this operation before further creation".into())),
                Err(error) => (TeamMemberCreatePhase::NeedsInspection, Some(error)),
            };
            return service
                .finish_member_creation(&params, phase, error)
                .await
                .map_err(|error| internal_error(error.to_string()));
        }

        let cwd = PathBuf::from(&params.cwd);
        let valid_cwd = cwd.is_absolute()
            && tokio::fs::metadata(&cwd)
                .await
                .is_ok_and(|metadata| metadata.is_dir());
        if !valid_cwd {
            return service
                .finish_member_creation(
                    &params,
                    TeamMemberCreatePhase::Failed,
                    Some(
                        "member checkout must be an existing absolute directory on this worker"
                            .into(),
                    ),
                )
                .await
                .map_err(|error| internal_error(error.to_string()));
        }
        let overrides = ConfigOverrides {
            model: params.model.clone(),
            cwd: Some(cwd),
            model_reasoning_effort: params.effort.clone().map(Some),
            ephemeral: Some(false),
            ..Default::default()
        };
        let manager = Arc::clone(&context.thread_manager);
        let recovery_store = Arc::clone(&thread_store);
        let started = Self::thread_start_task(
            context,
            thread_store,
            config_manager,
            request_id,
            client_name,
            client_version,
            client_mcp_extensions,
            None,
            overrides,
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            trace,
            warnings,
            ThreadStartupTarget::Member(Box::new(MemberStartup {
                intent,
                service: Arc::clone(&service),
                auth_manager,
            })),
        )
        .await;
        let (phase, error) = match started {
            Ok(_) => (TeamMemberCreatePhase::Created, None),
            Err(error) => {
                let current = service
                    .prepare_member_creation(params.clone())
                    .await
                    .map_err(|error| internal_error(error.to_string()))?;
                if current.starting {
                    match recover_member_root(&manager, recovery_store.as_ref(), thread_id).await {
                        Ok(true) => (TeamMemberCreatePhase::Created, None),
                        Ok(false) | Err(_) => {
                            (TeamMemberCreatePhase::NeedsInspection, Some(error.message))
                        }
                    }
                } else {
                    (TeamMemberCreatePhase::Failed, Some(error.message))
                }
            }
        };
        service
            .finish_member_creation(&params, phase, error)
            .await
            .map_err(|error| internal_error(error.to_string()))
    }
}

async fn recover_member_root(
    manager: &ThreadManager,
    store: &dyn ThreadStore,
    thread_id: ThreadId,
) -> Result<bool, String> {
    if let Ok(thread) = manager.get_thread(thread_id).await {
        store
            .persist_thread(thread_id, PersistContext::Standard)
            .await
            .map_err(|error| format!("cannot materialize reserved member root: {error}"))?;
        // Recovery must commit the same initial metadata as fresh creation,
        // including the empty paginated history before any model turn exists.
        thread
            .update_thread_metadata(
                StoreThreadMetadataPatch::default(),
                /*include_archived*/ true,
            )
            .await
            .map_err(|error| format!("cannot materialize reserved member metadata: {error}"))?;
        thread
            .flush_rollout()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(true);
    }
    match store
        .read_thread(codex_thread_store::ReadThreadParams {
            thread_id,
            include_history: false,
            include_archived: true,
        })
        .await
    {
        Ok(_) => Ok(true),
        Err(ThreadStoreError::ThreadNotFound { .. }) => Ok(false),
        Err(error) => Err(format!("cannot recover reserved member root: {error}")),
    }
}
