//! Alias reservations release on cancellation without locking another host's RPC.

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::coordinator::request_id;
use crate::coordinator::status_text;
use crate::model::*;
use crate::thread_metadata::ThreadReadMetadata;
use crate::thread_metadata::ThreadResumeMetadata;

#[derive(Default)]
pub(crate) struct AliasReservations(Mutex<HashSet<String>>);

struct AliasReservation<'a> {
    owner: &'a AliasReservations,
    alias: String,
}

impl AliasReservations {
    fn reserve(&self, alias: String) -> Result<AliasReservation<'_>> {
        let mut reservations = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            reservations.insert(alias.clone()),
            "agent alias enrollment is already pending"
        );
        Ok(AliasReservation { owner: self, alias })
    }
}

impl Drop for AliasReservation<'_> {
    fn drop(&mut self) {
        self.owner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.alias);
    }
}

impl Coordinator {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn enroll(
        &self,
        address: TeamAddress,
        alias: String,
        operation_id: Option<&str>,
    ) -> Result<Agent> {
        let address = canonical_address(address)?;
        // Alias reservation spans the worker RPC without holding the control
        // lock: an unreachable worker must never prevent a team-wide Pause.
        ensure!(!alias.trim().is_empty(), "an agent alias is required");
        ensure!(!self.agents().await?.iter().any(|agent| agent.enrollment.alias == alias && agent.enrollment.address != address), "agent alias is already in use");
        let _reservation = self.enrollments.reserve(alias.clone())?;
        self.enrollment_intent(&address, &alias, operation_id)
            .await?;
        let peer = self.peer(&address.machine_id, &address.server_id).await?;
        let mut cursor = None;
        let loaded = loop {
            let page: ThreadLoadedListResponse = peer
                .handle
                .request_typed(ClientRequest::ThreadLoadedList {
                    request_id: request_id(),
                    params: ThreadLoadedListParams {
                        cursor,
                        limit: Some(1000),
                    },
                })
                .await?;
            if page.data.contains(&address.thread_id) {
                break true;
            }
            let Some(next) = page.next_cursor else {
                break false;
            };
            cursor = Some(next);
        };
        if !loaded {
            let _: ThreadResumeMetadata = peer
                .handle
                .request_typed(ClientRequest::ThreadResume {
                    request_id: request_id(),
                    params: ThreadResumeParams {
                        thread_id: address.thread_id.clone(),
                        ..Default::default()
                    },
                })
                .await?;
        }
        let enrolled: TeamEnrollResponse = peer
            .handle
            .request_typed(ClientRequest::TeamEnroll {
                request_id: request_id(),
                params: TeamEnrollParams {
                    operation_id: operation_id.map(str::to_owned),
                    address: address.clone(),
                    alias,
                    control: self.current_control().await?,
                },
            })
            .await?;
        let key = address_key(&address);
        let read: ThreadReadMetadata = peer
            .handle
            .request_typed(ClientRequest::ThreadRead {
                request_id: request_id(),
                params: ThreadReadParams {
                    thread_id: address.thread_id.clone(),
                    include_turns: false,
                },
            })
            .await?;
        let agent = Agent {
            enrollment: enrolled.enrollment,
            requested_selection: None,
            runtime: None,
            status: status_text(&read.thread.status),
            current_model: None,
            current_effort: None,
            next_model: None,
            next_effort: None,
            project: read.thread.project_id,
        };
        let _mutation = self.mutations.lock().await;
        if self.enrollment_is_latest(&address, operation_id).await? {
            self.store.put("agent", &key, &agent).await?;
            *self
                .observed_versions
                .lock()
                .await
                .entry(server_key(&address.machine_id, &address.server_id))
                .or_default() += 1;
            self.notify();
        }
        Ok(agent)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the destination model lock fences leave against pending RPCs; the mutation lock orders durable membership and mail cleanup"
    )]
    pub(crate) async fn leave(
        &self,
        address: TeamAddress,
        operation_id: Option<&str>,
    ) -> Result<()> {
        let address = canonical_address(address)?;
        // Finish this destination's admitted model requests before revoking
        // membership, so captured intent cannot be sent after leave succeeds.
        let model_updates = self
            .model_update_lock(&address.machine_id, &address.server_id)
            .await;
        let _model_update = model_updates.lock().await;
        self.enrollment_intent(&address, "leave", operation_id)
            .await?;
        let mut departed = std::collections::HashSet::from([address_key(&address)]);
        for agent in self.agents().await? {
            let member = &agent.enrollment.address;
            if member.machine_id == address.machine_id
                && member.server_id == address.server_id
                && agent
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.root_thread_id == address.thread_id)
            {
                departed.insert(address_key(member));
            }
        }
        let peer = self.peer(&address.machine_id, &address.server_id).await?;
        let _: TeamLeaveResponse = peer
            .handle
            .request_typed(ClientRequest::TeamLeave {
                request_id: request_id(),
                params: TeamLeaveParams {
                    coordinator_id: self.current_control().await?.coordinator_id,
                    thread_id: address.thread_id.clone(),
                    operation_id: operation_id.map(str::to_owned),
                },
            })
            .await?;
        let _mutation = self.mutations.lock().await;
        if self.enrollment_is_latest(&address, operation_id).await? {
            for agent in self.agents().await? {
                let member = &agent.enrollment.address;
                if member.machine_id == address.machine_id
                    && member.server_id == address.server_id
                    && (member.thread_id == address.thread_id
                        || agent
                            .runtime
                            .as_ref()
                            .is_some_and(|runtime| runtime.root_thread_id == address.thread_id))
                {
                    departed.insert(address_key(member));
                }
            }
            self.forget_agents(&departed).await?;
            for (id, mut record) in self
                .store
                .list::<MessageRecord>("message", "", u32::MAX)
                .await?
            {
                let targets = record
                    .targets
                    .clone()
                    .unwrap_or_else(|| record.message.recipient.clone().into_iter().collect());
                let mut changed = false;
                for target in targets {
                    if !departed.contains(&address_key(&target)) {
                        continue;
                    }
                    let delivery_id = format!("{id}:{}", address_key(&target));
                    if record
                        .receipts
                        .get(&delivery_id)
                        .is_some_and(|receipt| receipt.state != TeamReceiptState::Pending)
                    {
                        continue;
                    }
                    record.receipts.insert(
                        delivery_id.clone(),
                        TeamReceipt {
                            delivery_id,
                            thread_id: target.thread_id,
                            state: TeamReceiptState::Rejected,
                            turn_id: None,
                            detail: Some("Recipient left the team before delivery".into()),
                        },
                    );
                    changed = true;
                }
                if changed {
                    self.store.put("message", &id, &record).await?;
                }
            }
            *self
                .observed_versions
                .lock()
                .await
                .entry(server_key(&address.machine_id, &address.server_id))
                .or_default() += 1;
            self.notify();
        }
        Ok(())
    }

    pub(crate) async fn forget_agents(
        &self,
        departed: &std::collections::HashSet<String>,
    ) -> Result<()> {
        // Remove pending selection intent first. A crash during cleanup may
        // leave a stale directory row, but cannot replay old settings on rejoin.
        for key in departed {
            self.store.delete("model_request", key).await?;
        }
        for key in departed {
            self.store.delete("agent", key).await?;
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "orders the durable latest enrollment intent with membership mutations"
    )]
    async fn enrollment_intent(
        &self,
        address: &TeamAddress,
        alias: &str,
        operation_id: Option<&str>,
    ) -> Result<()> {
        if let Some(id) = operation_id {
            let _mutation = self.mutations.lock().await;
            self.store
                .put_record_once(
                    "enrollment_intent",
                    id,
                    &serde_json::to_string(&(address, alias))?,
                    &[(
                        "enrollment_latest",
                        &address_key(address),
                        serde_json::to_value(id)?,
                    )],
                )
                .await?;
        }
        Ok(())
    }

    async fn enrollment_is_latest(
        &self,
        address: &TeamAddress,
        operation_id: Option<&str>,
    ) -> Result<bool> {
        match operation_id {
            Some(id) => Ok(self
                .store
                .get::<String>("enrollment_latest", &address_key(address))
                .await?
                .as_deref()
                == Some(id)),
            None => Ok(true),
        }
    }
}
