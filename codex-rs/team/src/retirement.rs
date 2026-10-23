//! Membership retirement is separate from operator execution controls.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;

use crate::coordinator::Coordinator;
use crate::model::*;

impl Coordinator {
    pub(crate) async fn member_is_retiring(&self, address: &TeamAddress) -> Result<bool> {
        let mut root = address.clone();
        if let Some(agent) = self
            .store
            .get::<Agent>("agent", &address_key(address))
            .await?
            && let Some(runtime) = agent.runtime
        {
            root.thread_id = runtime.root_thread_id;
        }
        Ok(self
            .store
            .get::<TeamMemberRetirement>("retirement", &address_key(&root))
            .await?
            .is_some_and(|state| state.state == TeamMemberRetirementState::Retiring))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "retirement closes new intake in the coordinator mutation order"
    )]
    pub(crate) async fn retire_member(
        &self,
        address: TeamAddress,
        operation_id: &str,
    ) -> Result<TeamMemberRetirement> {
        let address = canonical_address(address)?;
        let _mutation = self.mutations.lock().await;
        let key = address_key(&address);
        if let Some(previous) = self
            .store
            .get::<TeamMemberRetirement>("retirement", &key)
            .await?
        {
            if previous.operation_id == operation_id {
                return Ok(previous);
            }
            // Distinct requests must not claim ownership of the first request's
            // retirement and then fail only after that request completes.
            let existing_operation_id = &previous.operation_id;
            ensure!(
                previous.state != TeamMemberRetirementState::Retiring,
                "member is already retiring under operation {existing_operation_id}"
            );
        }
        let agent = self
            .store
            .get::<Agent>("agent", &key)
            .await?
            .context("member to remove is not enrolled")?;
        ensure!(
            agent
                .runtime
                .as_ref()
                .is_none_or(|runtime| runtime.root_thread_id == address.thread_id),
            "remove the enrolled root to retire its inherited family"
        );
        let retirement = TeamMemberRetirement {
            operation_id: operation_id.into(),
            address,
            state: TeamMemberRetirementState::Retiring,
            waiting_reason: Some("Waiting for the worker to drain admitted work.".into()),
        };
        self.store.put("retirement", &key, &retirement).await?;
        self.notify();
        Ok(retirement)
    }

    pub(crate) async fn retirement_intents(
        &self,
        machine: &str,
        server: &str,
    ) -> Result<Vec<TeamMemberRetirement>> {
        Ok(self
            .store
            .list::<TeamMemberRetirement>("retirement", "", u32::MAX)
            .await?
            .into_iter()
            .map(|(_, retirement)| retirement)
            .filter(|retirement| {
                retirement.address.machine_id == machine
                    && retirement.address.server_id == server
                    && retirement.state == TeamMemberRetirementState::Retiring
            })
            .collect())
    }

    /// Run after this host's receipt outbox was drained. Missing receipt evidence
    /// then describes inputs that never reached an admitted host turn.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "worker retirement acknowledgement and membership removal commit in one mutation order"
    )]
    pub(crate) async fn observe_retirements(
        &self,
        machine: &str,
        server: &str,
        observations: &[TeamMemberRetirement],
    ) -> Result<()> {
        let _mutation = self.mutations.lock().await;
        for observed in observations {
            ensure!(
                observed.address.machine_id == machine && observed.address.server_id == server,
                "retirement observation belongs to another worker"
            );
            let key = address_key(&observed.address);
            let Some(previous) = self
                .store
                .get::<TeamMemberRetirement>("retirement", &key)
                .await?
            else {
                continue;
            };
            if previous.operation_id != observed.operation_id
                || previous == *observed
                || previous.state == TeamMemberRetirementState::Removed
            {
                continue;
            }
            if observed.state == TeamMemberRetirementState::Removed {
                let departed = self
                    .store
                    .list::<Agent>("agent", "", u32::MAX)
                    .await?
                    .into_iter()
                    .filter(|(_, agent)| {
                        agent.enrollment.address.machine_id == machine
                            && agent.enrollment.address.server_id == server
                            && (agent.enrollment.address.thread_id == observed.address.thread_id
                                || agent.runtime.as_ref().is_some_and(|runtime| {
                                    runtime.root_thread_id == observed.address.thread_id
                                }))
                    })
                    .map(|(key, _)| key)
                    .collect::<std::collections::HashSet<_>>();
                for (id, mut record) in self
                    .store
                    .list::<MessageRecord>("message", "", u32::MAX)
                    .await?
                {
                    let mut changed = false;
                    // Direct mail may still be awaiting publisher routing. Its
                    // explicit recipient is already fixed, including removal.
                    if record.targets.is_none()
                        && let Some(target) = &record.message.recipient
                        && departed.contains(&address_key(target))
                    {
                        record.targets = Some(vec![target.clone()]);
                        changed = true;
                    }
                    for target in record.targets.iter().flatten() {
                        if !departed.contains(&address_key(target)) {
                            continue;
                        }
                        let delivery_id = format!("{}:{}", record.message.id, address_key(target));
                        if record
                            .receipts
                            .get(&delivery_id)
                            .is_none_or(|receipt| receipt.state == TeamReceiptState::Pending)
                        {
                            record.receipts.insert(
                                delivery_id.clone(),
                                TeamReceipt {
                                    delivery_id,
                                    thread_id: target.thread_id.clone(),
                                    state: TeamReceiptState::Rejected,
                                    turn_id: None,
                                    detail: Some(
                                        "Recipient was removed before this input was admitted."
                                            .into(),
                                    ),
                                },
                            );
                            changed = true;
                        }
                    }
                    if changed {
                        self.store.put("message", &id, &record).await?;
                    }
                }
                self.forget_agents(&departed).await?;
            }
            self.store.put("retirement", &key, observed).await?;
            self.notify();
        }
        Ok(())
    }
}
