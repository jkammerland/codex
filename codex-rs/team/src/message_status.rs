//! Read-only delivery evidence; transport acknowledgement is not acceptance.

use anyhow::Result;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::model::*;

/// Project the same evidence for the CLI and TUI without interpreting replies as success.
pub fn message_statuses(snapshot: &TeamSnapshot) -> Vec<TeamMessageStatus> {
    let name = |address: &TeamAddress| {
        snapshot
            .agents
            .iter()
            .find(|agent| agent.enrollment.address == *address)
            .map(|agent| agent.enrollment.alias.clone())
            .unwrap_or_else(|| match address.machine_id.as_str() {
                "operator" => "You".into(),
                "gitea" => format!("Gitea · {}", address.thread_id),
                _ => address_key(address),
            })
    };
    let mut data = snapshot
        .messages
        .iter()
        .map(|record| {
            let targets = record
                .targets
                .clone()
                .unwrap_or_else(|| record.message.recipient.iter().cloned().collect());
            TeamMessageStatus {
                message_id: record.message.id.clone(),
                sender: record.message.sender.clone(),
                sender_name: name(&record.message.sender),
                created_at: Some(record.message.created_at),
                intake: TeamMessageIntake::AcceptedAtCoordinator,
                recipients: targets
                    .into_iter()
                    .map(|address| {
                        let receipt = record
                            .receipts
                            .get(&format!("{}:{}", record.message.id, address_key(&address)))
                            .cloned();
                        let freshness = if snapshot.servers.iter().any(|server| {
                            server.machine_id == address.machine_id
                                && server.server_id == address.server_id
                                && server.connected
                        }) {
                            TeamObservationFreshness::Current
                        } else {
                            TeamObservationFreshness::Unconfirmed
                        };
                        TeamRecipientMessageStatus {
                            name: name(&address),
                            abandonment: record
                                .abandonments
                                .get(&format!("{}:{}", record.message.id, address_key(&address)))
                                .cloned(),
                            address,
                            progress: receipt
                                .as_ref()
                                .map_or(TeamMessageProgress::QueuedAtCoordinator, |receipt| {
                                    receipt.state.into()
                                }),
                            receipt,
                            freshness,
                        }
                    })
                    .collect(),
                related_replies: snapshot
                    .messages
                    .iter()
                    .filter(|reply| reply.message.reply_to.as_ref() == Some(&record.message.id))
                    .map(|reply| reply.message.id.clone())
                    .collect(),
                detail: record.error.clone(),
                freshness: TeamObservationFreshness::Current,
            }
        })
        .collect::<Vec<_>>();

    // Rejections are durably acknowledged outbox entries too. They must win
    // over a sender's older local "queued" observation after synchronization.
    for rejected in &snapshot.rejected_outbox {
        let payload = &rejected.entry.payload;
        let message = payload
            .get("message")
            .and_then(|value| serde_json::from_value::<TeamMessage>(value.clone()).ok());
        let sender = message
            .as_ref()
            .map(|message| message.sender.clone())
            .or_else(|| {
                payload
                    .get("sender")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
            });
        let Some(sender) = sender.filter(|sender| {
            sender.machine_id == rejected.machine_id && sender.server_id == rejected.server_id
        }) else {
            continue;
        };
        if !matches!(rejected.entry.kind.as_str(), "message" | "board") {
            continue;
        }
        if rejected.entry.kind == "board"
            && !matches!(
                payload
                    .pointer("/request/type")
                    .and_then(serde_json::Value::as_str),
                Some("post" | "reply")
            )
        {
            continue;
        }
        data.push(TeamMessageStatus {
            message_id: message
                .as_ref()
                .map_or_else(|| rejected.entry.id.clone(), |message| message.id.clone()),
            sender_name: name(&sender),
            sender,
            created_at: message.as_ref().map(|message| message.created_at),
            intake: TeamMessageIntake::Rejected,
            recipients: Vec::new(),
            related_replies: Vec::new(),
            detail: Some(rejected.reason.clone()),
            freshness: TeamObservationFreshness::Current,
        });
    }
    data.sort_by(|left, right| left.message_id.cmp(&right.message_id));
    data
}

impl Coordinator {
    pub(crate) async fn message_status(
        &self,
        query: TeamMessageStatusParams,
    ) -> Result<TeamMessageStatusResponse> {
        let mut data = message_statuses(&self.snapshot().await?)
            .into_iter()
            .filter(|status| {
                query
                    .message_id
                    .as_ref()
                    .is_none_or(|id| id == &status.message_id)
            })
            .filter(|status| {
                query
                    .cursor
                    .as_ref()
                    .is_none_or(|cursor| &status.message_id > cursor)
            })
            .collect::<Vec<_>>();
        let limit = query.limit.unwrap_or(100).clamp(1, 1000) as usize;
        let next_cursor = if data.len() > limit {
            data.truncate(limit);
            data.last().map(|status| status.message_id.clone())
        } else {
            None
        };
        Ok(TeamMessageStatusResponse { data, next_cursor })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "content revision and cache are committed under the coordinator mutation fence"
    )]
    pub(crate) async fn message_status_snapshot(&self) -> Result<TeamMessageStatusSnapshot> {
        let _mutation = self.mutations.lock().await;
        let servers = self.statuses.read().await.values().cloned().collect();
        let agents = self.agents().await?;
        // This projection does not need Board state, whose read acquires the
        // same mutation fence. Read only message evidence while holding it.
        let data = message_statuses(&TeamSnapshot {
            operations: Vec::new(),
            control: self.current_control().await?,
            servers,
            agents,
            messages: self
                .store
                .list::<MessageRecord>("message", "", u32::MAX)
                .await?
                .into_iter()
                .map(|(_, value)| value)
                .collect(),
            rejected_outbox: self
                .store
                .list::<RejectedOutbox>("rejected_outbox", "", u32::MAX)
                .await?
                .into_iter()
                .map(|(_, value)| value)
                .collect(),
            board: Default::default(),
        });
        let previous = self
            .store
            .get::<TeamMessageStatusSnapshot>("message_status_cache", "current")
            .await?
            .unwrap_or_default();
        if previous.data == data {
            return Ok(previous);
        }
        let snapshot = TeamMessageStatusSnapshot {
            revision: previous
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("message status revision exhausted"))?,
            data,
        };
        self.store
            .put("message_status_cache", "current", &snapshot)
            .await?;
        Ok(snapshot)
    }
}

#[cfg(test)]
#[path = "message_status_tests.rs"]
mod tests;
