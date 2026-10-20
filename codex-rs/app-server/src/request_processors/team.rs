use codex_app_server_protocol::TeamAgentCredentialParams;
use codex_app_server_protocol::TeamAgentCredentialResponse;
use codex_app_server_protocol::TeamAgentRequestParams;
use codex_app_server_protocol::TeamAgentRequestResponse;
use codex_app_server_protocol::TeamOutboxAckParams;
use codex_app_server_protocol::TeamOutboxAckResponse;
use codex_app_server_protocol::TeamOutboxListParams;
use codex_app_server_protocol::TeamOutboxListResponse;
use codex_app_server_protocol::TeamSyncParams;
use codex_app_server_protocol::TeamSyncResponse;
use std::sync::Arc;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TeamChangedNotification;
use codex_app_server_protocol::TeamControlParams;
use codex_app_server_protocol::TeamControlResponse;
use codex_app_server_protocol::TeamDeliverParams;
use codex_app_server_protocol::TeamDeliverResponse;
use codex_app_server_protocol::TeamEnrollParams;
use codex_app_server_protocol::TeamEnrollResponse;
use codex_app_server_protocol::TeamHostReadParams;
use codex_app_server_protocol::TeamHostReadResponse;
use codex_team_extension::TeamHostService;

use crate::error_code::invalid_request;
use crate::outgoing_message::OutgoingMessageSender;

pub(crate) struct TeamRequestProcessor {
    service: Arc<TeamHostService>,
    thread_manager: Arc<codex_core::ThreadManager>,
    thread_store: Arc<dyn codex_thread_store::ThreadStore>,
    thread_watch: crate::thread_status::ThreadWatchManager,
}

impl TeamRequestProcessor {
    pub(crate) fn new(
        service: Arc<TeamHostService>,
        thread_manager: Arc<codex_core::ThreadManager>,
        thread_store: Arc<dyn codex_thread_store::ThreadStore>,
        outgoing: Arc<OutgoingMessageSender>,
        thread_watch: crate::thread_status::ThreadWatchManager,
    ) -> Self {
        let mut changes = service.subscribe();
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let revision = *changes.borrow_and_update();
                outgoing
                    .send_server_notification(ServerNotification::TeamChanged(
                        TeamChangedNotification { revision },
                    ))
                    .await;
            }
        });
        Self {
            service,
            thread_manager,
            thread_store,
            thread_watch,
        }
    }

    pub(crate) async fn outbox_list(
        &self,
        params: TeamOutboxListParams,
    ) -> Result<TeamOutboxListResponse, JSONRPCErrorError> {
        self.service
            .outbox_list(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn outbox_ack(
        &self,
        params: TeamOutboxAckParams,
    ) -> Result<TeamOutboxAckResponse, JSONRPCErrorError> {
        self.service
            .outbox_ack(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn sync(
        &self,
        params: TeamSyncParams,
    ) -> Result<TeamSyncResponse, JSONRPCErrorError> {
        self.service
            .sync(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn agent_request(
        &self,
        params: TeamAgentRequestParams,
    ) -> Result<TeamAgentRequestResponse, JSONRPCErrorError> {
        self.service
            .agent_request(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn agent_credential(
        &self,
        params: TeamAgentCredentialParams,
    ) -> Result<TeamAgentCredentialResponse, JSONRPCErrorError> {
        self.service
            .agent_credential(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn read(
        &self,
        params: TeamHostReadParams,
    ) -> Result<TeamHostReadResponse, JSONRPCErrorError> {
        let mut response = self
            .service
            .read(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
        let statuses = self
            .thread_watch
            .loaded_statuses_for_threads(
                response
                    .runtime
                    .iter()
                    .filter(|runtime| runtime.loaded)
                    .map(|runtime| runtime.thread_id.clone()),
            )
            .await;
        for runtime in &mut response.runtime {
            if let Some(status) = statuses.get(&runtime.thread_id) {
                runtime.activity = status.into();
            }
        }
        Ok(response)
    }

    pub(crate) async fn enroll(
        &self,
        params: TeamEnrollParams,
    ) -> Result<TeamEnrollResponse, JSONRPCErrorError> {
        self.service
            .enroll(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) fn leave(
        &self,
        params: codex_app_server_protocol::TeamLeaveParams,
    ) -> futures::future::BoxFuture<
        '_,
        Result<codex_app_server_protocol::TeamLeaveResponse, JSONRPCErrorError>,
    > {
        // Leave checkpoints live thread settings. Keep its large future out of
        // the shared dispatcher so ordinary resume requests retain stack space.
        Box::pin(async move {
            self.service
                .leave(params)
                .await
                .map_err(|error| invalid_request(error.to_string()))
        })
    }

    pub(crate) async fn control(
        &self,
        params: TeamControlParams,
    ) -> Result<TeamControlResponse, JSONRPCErrorError> {
        self.service
            .control(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn deliver(
        &self,
        params: TeamDeliverParams,
    ) -> Result<TeamDeliverResponse, JSONRPCErrorError> {
        self.service
            .deliver(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }

    pub(crate) async fn defer_model_selection(
        &self,
        params: &codex_app_server_protocol::ThreadSettingsUpdateParams,
    ) -> Result<bool, JSONRPCErrorError> {
        if params.team_selection.is_none() {
            return Ok(false);
        }
        let thread_id = codex_protocol::ThreadId::from_string(&params.thread_id)
            .map_err(|error| invalid_request(error.to_string()))?;
        if self.thread_manager.get_thread(thread_id).await.is_ok() {
            return Ok(false);
        }
        let stored = self
            .thread_store
            .read_thread(codex_thread_store::ReadThreadParams {
                thread_id,
                include_history: false,
                include_archived: false,
            })
            .await
            .map_err(|error| invalid_request(error.to_string()))?;

        // Unloaded children retain the same parent-ownership restriction as
        // the loaded settings and resume paths.
        if matches!(
            &stored.source,
            codex_protocol::protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. }
            )
        ) {
            let context = self
                .thread_store
                .load_latest_model_context(codex_thread_store::LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await
                .map_err(|error| invalid_request(error.to_string()))?;
            let version = codex_history::InitialHistory::Resumed(codex_history::ResumedHistory {
                conversation_id: thread_id,
                history: Arc::new(context.items),
                rollout_path: stored.rollout_path,
                history_revision: context.revision,
            })
            .get_multi_agent_version();
            if !super::thread_input::can_accept_direct_input(version, &stored.source) {
                return Err(invalid_request(
                    super::thread_input::DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR,
                ));
            }
        }
        self.service
            .defer_model_selection(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
    }
}
