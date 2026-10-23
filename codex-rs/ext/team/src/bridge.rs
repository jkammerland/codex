use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::TeamHostService;

use crate::discovery::CachedDirectory;
use crate::discovery::page;
use crate::publish::Publication;

impl TeamHostService {
    pub async fn outbox_list(
        &self,
        params: TeamOutboxListParams,
    ) -> Result<TeamOutboxListResponse> {
        let store = self.require_coordinator(&params.coordinator_id).await?;
        let after = params.cursor.as_deref().unwrap_or("0").parse::<i64>()?;
        let limit = params.limit.unwrap_or(100).clamp(1, 1000);
        let mut data = store.outbox(after, limit + 1).await?;
        let next_cursor = if data.len() > limit as usize {
            data.truncate(limit as usize);
            data.last().map(|entry| entry.sequence.to_string())
        } else {
            None
        };
        Ok(TeamOutboxListResponse {
            data: data
                .into_iter()
                .map(|entry| TeamOutboxEntry {
                    sequence: entry.sequence,
                    id: entry.id,
                    kind: entry.kind,
                    payload: entry.payload,
                })
                .collect(),
            next_cursor,
        })
    }

    pub async fn outbox_ack(&self, params: TeamOutboxAckParams) -> Result<TeamOutboxAckResponse> {
        self.require_coordinator(&params.coordinator_id)
            .await?
            .acknowledge(&params.sequences)
            .await?;
        Ok(TeamOutboxAckResponse {})
    }

    #[allow(
        clippy::await_holding_invalid_type,
        reason = "publishes a committed Board snapshot before dispatch observes it"
    )]
    pub async fn sync(&self, mut params: TeamSyncParams) -> Result<TeamSyncResponse> {
        let _operation = self.operations.lock().await;
        let store = self.require_coordinator(&params.coordinator_id).await?;
        // Purge admission shares the dispatch lock. Only content-free receipt
        // updates may survive after this transaction acknowledges removal.
        let board_purge_acknowledgements = store.apply_board_purges(&params.board_purges).await?;
        store
            .filter_removed_board_snapshot(&mut params.board_state)
            .await?;
        store
            .filter_removed_message_status(&mut params.message_status)
            .await?;
        if let Some(previous) = store
            .get::<TeamMessageStatusSnapshot>("message_status_cache", "current")
            .await?
        {
            ensure!(
                params.message_status.revision >= previous.revision,
                "message status snapshot revision must not regress"
            );
            ensure!(
                params.message_status.revision != previous.revision
                    || params.message_status == previous,
                "message status snapshot revision has conflicting content"
            );
        }
        let previous = store
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?;
        if let Some(previous) = &previous {
            ensure!(
                params.board_state.revision >= previous.revision,
                "board snapshot revision must not regress"
            );
        }
        let entries_changed = previous
            .as_ref()
            .is_none_or(|previous| previous.entries != params.board_state.entries);
        for adapter in &params.cancellation_adapters {
            ensure!(
                adapter.cancel_arguments.is_object()
                    && !adapter.cancel_id_field.is_empty()
                    && adapter.job_id_pointer.starts_with('/')
                    && adapter.confirmation_pointer.starts_with('/'),
                "invalid cancellation adapter"
            );
        }
        let previous_directory = store
            .get::<TeamDirectorySnapshot>("directory_cache", "current")
            .await?;
        if let Some(previous) = previous_directory {
            ensure!(
                params.directory.revision >= previous.revision,
                "directory snapshot revision must not regress"
            );
            ensure!(
                params.directory.revision != previous.revision || params.directory == previous,
                "directory snapshot changed without a new revision"
            );
        }
        store
            .put(
                "configuration",
                "cancellation_adapters",
                &params.cancellation_adapters,
            )
            .await?;
        // One durable record replaces the complete directory, including removals.
        let cached_directory = CachedDirectory {
            directory: params.directory,
            last_synchronized_at: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_secs()
                    .try_into()?,
            ),
        };
        store
            .put("directory_cache", "current", &cached_directory)
            .await?;
        store
            .put("message_status_cache", "current", &params.message_status)
            .await?;
        for message in params.board {
            if store.board_message_removed(&message.id).await? {
                continue;
            }
            if let Some(topic) = &message.topic {
                store
                    .put(&format!("board:{topic}"), &message.id, &message)
                    .await?;
            }
        }
        store
            .put("board_cache", "current", &params.board_state)
            .await?;
        self.sync_administration(&params.administration, &params.retirements)
            .await?;
        // Sync acknowledges coordinator state; echoing TeamChanged would feed
        // another sync indefinitely. Only new entry metadata can unblock mail.
        if entries_changed {
            let _ = self.events.send(crate::lifecycle::HostEvent::Dispatch);
        }
        Ok(TeamSyncResponse {
            board_purge_acknowledgements,
        })
    }

    pub async fn agent_credential(
        &self,
        params: TeamAgentCredentialParams,
    ) -> Result<TeamAgentCredentialResponse> {
        let thread_id = ThreadId::from_string(&params.thread_id)?.to_string();
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let credential = match store.credential(&thread_id).await? {
            Some(credential) => credential,
            None => store
                .get::<String>("member_credential", &thread_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("agent is not enrolled"))?,
        };
        Ok(TeamAgentCredentialResponse { credential })
    }

    pub async fn agent_request(
        &self,
        params: TeamAgentRequestParams,
    ) -> Result<TeamAgentRequestResponse> {
        let expected = self
            .agent_credential(TeamAgentCredentialParams {
                thread_id: ThreadId::from_string(&params.thread_id)?.to_string(),
            })
            .await?
            .credential;
        ensure!(expected == params.credential, "invalid agent credential");
        self.agent_action(
            ThreadId::from_string(&params.thread_id)?,
            params.action,
            params.operation_id,
        )
        .await
    }

    pub(crate) async fn agent_action(
        &self,
        thread_id: ThreadId,
        action: TeamAgentActionParams,
        operation_id: Option<String>,
    ) -> Result<TeamAgentRequestResponse> {
        let operation = operation_id
            .map(|id| (format!("{thread_id}:{id}"), agent_operation_digest(&action)))
            .map(|(id, digest)| digest.map(|digest| (id, digest)))
            .transpose()?;
        let root = self
            .gate
            .root(thread_id)
            .ok_or_else(|| anyhow::anyhow!("agent is not enrolled"))?;
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let enrollment = store
            .enrollment(&root.to_string())
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent enrollment is missing"))?;
        let sender = TeamAddress {
            thread_id: thread_id.to_string(),
            ..enrollment.address
        };
        // Resolve aliases only once. Retrying a durable call must recover the
        // original result even when the alias has since changed or disappeared.
        if let Some((id, digest)) = &operation
            && let Some(previous) = store.get::<Value>("agent_operation", id).await?
        {
            ensure!(
                previous["digest"].as_str() == Some(digest),
                "operation identity has conflicting content"
            );
            return Ok(TeamAgentRequestResponse {
                result: previous["result"].clone(),
            });
        }
        let result = match action {
            TeamAgentActionParams::Publish {
                body,
                topic,
                conversation_id,
                reply_to,
                mentions,
            } => {
                self.publish(
                    sender,
                    Publication {
                        body,
                        topic,
                        conversation_id,
                        reply_to,
                        mentions: mentions.unwrap_or_default(),
                    },
                    operation.as_ref(),
                )
                .await?
            }
            TeamAgentActionParams::Member { request } => {
                self.agent_member_action(&sender, request, operation.as_ref())
                    .await?
            }
            TeamAgentActionParams::Cleanup { request } => {
                self.agent_cleanup_action(&sender, request, operation.as_ref())
                    .await?
            }
            TeamAgentActionParams::MessageStatus { query } => {
                serde_json::to_value(self.agent_message_status(&sender, query).await?)?
            }
            TeamAgentActionParams::ExplainAction { query } => {
                serde_json::to_value(self.agent_action_explanation(&sender, query).await?)?
            }
            TeamAgentActionParams::BoardAction { request } => {
                self.board_action(sender, request, operation.as_ref())
                    .await?
            }
            TeamAgentActionParams::Reply {
                message_id,
                body,
                delivery,
            } => {
                let original = store
                    .deliveries(&thread_id.to_string())
                    .await?
                    .into_iter()
                    .find(|(delivery, _)| delivery.message.id == message_id)
                    .map(|(delivery, _)| delivery.message)
                    .ok_or_else(|| {
                        anyhow::anyhow!("original message is not in this agent's inbox")
                    })?;
                let board = original.kind == TeamMessageKind::Board;
                self.queue_message(
                    sender,
                    if board { None } else { Some(original.sender) },
                    body,
                    if board {
                        TeamMessageKind::Board
                    } else {
                        TeamMessageKind::Reply
                    },
                    Some(message_id),
                    original.topic,
                    delivery,
                    operation.as_ref(),
                )
                .await?
            }
            TeamAgentActionParams::Directory {
                cursor,
                limit,
                query,
                details,
            } => self.agent_directory(cursor, limit, query, details).await?,
            TeamAgentActionParams::Board {
                topic,
                cursor,
                limit,
            } => page(
                store
                    .list::<TeamMessage>(
                        &format!("board:{topic}"),
                        cursor.as_deref().unwrap_or(""),
                        limit.unwrap_or(100).clamp(1, 1000) + 1,
                    )
                    .await?,
                limit,
            ),
            TeamAgentActionParams::Inbox { cursor, limit } => {
                self.agent_inbox(&thread_id.to_string(), cursor, limit)
                    .await?
            }
            TeamAgentActionParams::Send {
                recipient,
                body,
                kind,
                reply_to,
                delivery,
            } => {
                ensure!(
                    kind != TeamMessageKind::Board,
                    "use publish for Board messages"
                );
                ensure!(
                    kind != TeamMessageKind::Note || delivery == TeamDeliveryMode::NextTurn,
                    "passive notes require nextTurn delivery"
                );
                let (recipient, _) = self.resolve_team_target(&recipient).await?;
                self.queue_message(
                    sender,
                    Some(recipient),
                    body,
                    kind,
                    reply_to,
                    None,
                    delivery,
                    operation.as_ref(),
                )
                .await?
            }
            TeamAgentActionParams::Post {
                topic,
                body,
                reply_to,
            } => {
                ensure!(!topic.trim().is_empty(), "board topic is required");
                self.queue_message(
                    sender,
                    None,
                    body,
                    TeamMessageKind::Board,
                    reply_to,
                    Some(topic),
                    TeamDeliveryMode::NextTurn,
                    operation.as_ref(),
                )
                .await?
            }
            TeamAgentActionParams::Subscribe { topic, enabled } => {
                ensure!(!topic.trim().is_empty(), "board topic is required");
                let id = Uuid::now_v7().to_string();
                let payload = json!({"sender": sender, "topic": topic, "enabled": enabled});
                let result = json!({"id": id, "state": "queued"});
                let result = if let Some((operation_id, digest)) = &operation {
                    store
                        .enqueue_agent_operation(
                            operation_id,
                            digest,
                            &id,
                            "subscription",
                            &payload,
                            &result,
                        )
                        .await?
                } else {
                    store.enqueue(&id, "subscription", &payload).await?;
                    result
                };
                self.notify();
                result
            }
        };
        Ok(TeamAgentRequestResponse { result })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "message fields mirror the wire protocol at the three action call sites"
    )]
    async fn queue_message(
        &self,
        sender: TeamAddress,
        mut recipient: Option<TeamAddress>,
        body: String,
        kind: TeamMessageKind,
        reply_to: Option<String>,
        topic: Option<String>,
        delivery: TeamDeliveryMode,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        ensure!(!body.trim().is_empty(), "message body is required");

        // External inbox IDs are not agent thread UUIDs. The coordinator
        // validates operator identity and Gitea conversation provenance.
        if let Some(recipient) = &mut recipient
            && !matches!(recipient.machine_id.as_str(), "operator" | "gitea")
        {
            recipient.thread_id = ThreadId::from_string(&recipient.thread_id)?.to_string();
        }

        let message = TeamMessage {
            id: Uuid::now_v7().to_string(),
            sender,
            recipient,
            kind,
            body,
            reply_to,
            topic,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_secs()
                .try_into()?,
        };
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let payload = json!({"message": message, "delivery": delivery});
        let mut result = json!({
            "id":message.id,"state":"queued","delivery":delivery,
            "route":if let Some(recipient) = &message.recipient { json!({"type":"direct","recipient":recipient}) } else {
                json!({"type":"board","conversationId":message.reply_to,"topic":message.topic})
            }
        });
        if message.kind == TeamMessageKind::Board {
            let snapshot = store
                .get::<TeamBoardSnapshot>("board_cache", "current")
                .await?
                .unwrap_or_default();
            let conversation_id = message
                .reply_to
                .as_ref()
                .and_then(|id| {
                    snapshot
                        .entries
                        .iter()
                        .find(|entry| &entry.message.id == id)
                })
                .map(|entry| entry.conversation_id.clone());
            let conversation_id =
                conversation_id.or_else(|| message.reply_to.is_none().then(|| message.id.clone()));
            result["route"]["conversationId"] = json!(conversation_id);
            result["route"]["replyTo"] = json!(message.reply_to);
            let identity = TeamBoardIdentity::Agent {
                address: message.sender.clone(),
            };
            result["subscriptionMode"] = json!(
                conversation_id
                    .as_ref()
                    .and_then(|id| snapshot
                        .followers
                        .iter()
                        .find(|follower| &follower.conversation_id == id
                            && follower.identity == identity))
                    .map_or(TeamBoardSubscriptionMode::Off, |follower| follower.mode)
            );
            result["topicSubscriptionMode"] = json!(
                snapshot
                    .topic_subscriptions
                    .iter()
                    .find(
                        |subscription| Some(&subscription.topic) == message.topic.as_ref()
                            && subscription.identity == identity
                    )
                    .map_or(TeamBoardSubscriptionMode::Off, |subscription| subscription
                        .mode)
            );
            result["notificationPolicy"] = json!("mentionsOrWakeSubscriptions");
            result["revision"] = json!(snapshot.revision);
            result["freshness"] = json!("lastSynchronized");
        }
        let result = if let Some((operation_id, digest)) = operation {
            store
                .enqueue_agent_operation(
                    operation_id,
                    digest,
                    &message.id,
                    "message",
                    &payload,
                    &result,
                )
                .await?
        } else {
            store.enqueue(&message.id, "message", &payload).await?;
            result
        };
        self.notify();
        Ok(result)
    }
}

fn agent_operation_digest(action: &TeamAgentActionParams) -> Result<String> {
    // Durable operation receipts used this exact field order before replies
    // could choose a boundary. Keep that identity across an upgrade/retry.
    if let TeamAgentActionParams::Reply {
        message_id,
        body,
        delivery: TeamDeliveryMode::NextTurn,
    } = action
    {
        #[derive(serde::Serialize)]
        struct LegacyReply<'a> {
            #[serde(rename = "type")]
            action: &'a str,
            #[serde(rename = "messageId")]
            message_id: &'a str,
            body: &'a str,
        }
        return Ok(serde_json::to_string(&LegacyReply {
            action: "reply",
            message_id,
            body,
        })?);
    }
    Ok(serde_json::to_string(action)?)
}
