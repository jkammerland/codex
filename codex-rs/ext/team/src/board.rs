//! Agent-facing Board reads use the last committed coordinator snapshot.

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::TeamHostService;

impl TeamHostService {
    pub(crate) async fn board_action(
        &self,
        sender: TeamAddress,
        request: TeamBoardParams,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        let identity = TeamBoardIdentity::Agent {
            address: sender.clone(),
        };
        let store = self
            .store
            .get()
            .ok_or_else(|| anyhow::anyhow!("team host is not enrolled"))?;
        let snapshot = store
            .get::<TeamBoardSnapshot>("board_cache", "current")
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("board has not synchronized with the coordinator yet")
            })?;

        // Credential-bound and native calls cannot impersonate another follower or reader.
        let requested_identity = match &request {
            TeamBoardParams::List { identity, .. }
            | TeamBoardParams::Read { identity, .. }
            | TeamBoardParams::Follow { identity, .. }
            | TeamBoardParams::MarkRead { identity, .. }
            | TeamBoardParams::TopicFollow { identity, .. }
            | TeamBoardParams::TopicOverview { identity, .. }
            | TeamBoardParams::SubscriptionSet { identity, .. } => identity.as_ref(),
            TeamBoardParams::Post { .. }
            | TeamBoardParams::Reply { .. }
            | TeamBoardParams::Followers { .. }
            | TeamBoardParams::Topics { .. } => None,
        };
        ensure!(
            requested_identity.is_none_or(|requested| requested == &identity),
            "agent board actions may only use their own identity"
        );

        match &request {
            TeamBoardParams::List { .. }
            | TeamBoardParams::Read { .. }
            | TeamBoardParams::Followers { .. }
            | TeamBoardParams::Topics { .. }
            | TeamBoardParams::TopicOverview { .. } => {
                return Ok(serde_json::to_value(codex_team_store::query_board(
                    &snapshot, &identity, &request,
                )?)?);
            }
            TeamBoardParams::Post {
                topic,
                body,
                mentions,
            } => {
                ensure!(!topic.trim().is_empty(), "board topic is required");
                validate_body(body, mentions)?;
            }
            TeamBoardParams::Reply { body, mentions, .. } => validate_body(body, mentions)?,
            TeamBoardParams::TopicFollow { topic, .. } => {
                ensure!(!topic.trim().is_empty(), "board topic is required")
            }
            TeamBoardParams::MarkRead { append_seq, .. } => {
                ensure!(*append_seq >= 0, "read sequence must not be negative")
            }
            TeamBoardParams::Follow { .. } => {}
            TeamBoardParams::SubscriptionSet { target, .. } => {
                if let TeamBoardSubscriptionTarget::Topic { topic } = target {
                    ensure!(!topic.trim().is_empty(), "board topic is required");
                }
            }
        }

        let conversation_id = match &request {
            TeamBoardParams::Reply {
                conversation_id, ..
            }
            | TeamBoardParams::Follow {
                conversation_id, ..
            }
            | TeamBoardParams::MarkRead {
                conversation_id, ..
            } => Some(conversation_id.clone()),
            TeamBoardParams::SubscriptionSet {
                target: TeamBoardSubscriptionTarget::Conversation { conversation_id },
                ..
            } => Some(conversation_id.clone()),
            _ => None,
        };
        let id = Uuid::now_v7().to_string();
        let result = serde_json::to_value(TeamBoardResponse::Mutation {
            revision: snapshot.revision,
            state: TeamBoardMutationState::Queued,
            conversation_id: if matches!(request, TeamBoardParams::Post { .. }) {
                Some(id.clone())
            } else {
                conversation_id
            },
            id: id.clone(),
        })?;
        let payload = json!({"sender": sender, "request": request});
        let result = if let Some((operation_id, digest)) = operation {
            store
                .enqueue_agent_operation(operation_id, digest, &id, "board", &payload, &result)
                .await?
        } else {
            store.enqueue(&id, "board", &payload).await?;
            result
        };
        self.notify();
        Ok(result)
    }
}

fn validate_body(body: &str, mentions: &[TeamBoardMention]) -> Result<()> {
    ensure!(!body.trim().is_empty(), "message body is required");
    for mention in mentions {
        ensure!(
            mention.start < mention.end
                && mention.display.starts_with('@')
                && body.get(mention.start as usize..mention.end as usize)
                    == Some(mention.display.as_str()),
            "mention range must match its displayed @name in the body"
        );
    }
    Ok(())
}
