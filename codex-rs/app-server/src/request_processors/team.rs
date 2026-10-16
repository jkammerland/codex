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
}

impl TeamRequestProcessor {
    pub(crate) fn new(service: Arc<TeamHostService>, outgoing: Arc<OutgoingMessageSender>) -> Self {
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
        Self { service }
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
        self.service
            .read(params)
            .await
            .map_err(|error| invalid_request(error.to_string()))
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
}
