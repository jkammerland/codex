//! Follow parent next-turn selections until a child receives its own selection.

use anyhow::Result;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Machine;
use crate::config::Server;
use crate::coordinator::Coordinator;
use crate::coordinator::Peer;
use crate::coordinator::request_id;
use crate::model::Agent;
use crate::model::address_key;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Selection {
    model: Option<String>,
    effort: Option<String>,
}

impl Selection {
    fn next(agent: &Agent) -> Self {
        Self {
            model: agent.next_model.clone(),
            effort: agent.next_effort.clone(),
        }
    }

    fn current(agent: &Agent) -> Self {
        Self {
            model: agent.current_model.clone(),
            effort: agent.current_effort.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Inheritance {
    parent: TeamAddress,
    applied: Selection,
    pending: Option<Selection>,
}

impl Coordinator {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "serializes durable state changes and dispatch across asynchronous operations"
    )]
    pub(crate) async fn inherit_models(
        &self,
        machine: &Machine,
        server: &Server,
        peer: &Peer,
    ) -> Result<()> {
        let selection = self.model_update_lock(&machine.id, &server.id).await;
        let _selection = selection.lock().await;
        let agents = self.agents().await?;
        for child in &agents {
            let address = &child.enrollment.address;
            if address.machine_id != machine.id || address.server_id != server.id {
                continue;
            }
            let Some(runtime) = &child.runtime else {
                continue;
            };
            let Some(parent_id) = &runtime.parent_thread_id else {
                continue;
            };
            if !runtime.loaded {
                continue;
            }
            let key = address_key(address);
            if self
                .store
                .get::<bool>("model_override", &key)
                .await?
                .unwrap_or(false)
            {
                continue;
            }
            let Some(parent) = agents.iter().find(|parent| {
                parent.enrollment.address
                    == TeamAddress {
                        thread_id: parent_id.clone(),
                        ..address.clone()
                    }
            }) else {
                continue;
            };
            let selected = Selection::next(child);
            let previous = self
                .store
                .get::<Inheritance>("model_inheritance", &key)
                .await?;
            let Some(mut inheritance) = reconcile_inheritance(parent, &selected, previous) else {
                self.store.put("model_override", &key, &true).await?;
                continue;
            };
            let desired = Selection::next(parent);
            if desired.model.is_none() {
                continue;
            }
            if desired != selected {
                // Journal before the RPC so its notification and a lost
                // acknowledgement cannot be mistaken for a child override.
                inheritance.pending = Some(desired.clone());
                self.store
                    .put("model_inheritance", &key, &inheritance)
                    .await?;
                let effort = desired
                    .effort
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                let _: ThreadSettingsUpdateResponse = peer
                    .handle
                    .request_typed(ClientRequest::ThreadSettingsUpdate {
                        request_id: request_id(),
                        params: ThreadSettingsUpdateParams {
                            thread_id: address.thread_id.clone(),
                            model: desired.model,
                            effort,
                            ..Default::default()
                        },
                    })
                    .await?;
            } else {
                self.store
                    .put("model_inheritance", &key, &inheritance)
                    .await?;
            }
        }
        Ok(())
    }
}

// Keep explicit local selections when reconnecting. A pending value is an
// acknowledgement of our own write, not a new user override.
fn reconcile_inheritance(
    parent: &Agent,
    selected: &Selection,
    previous: Option<Inheritance>,
) -> Option<Inheritance> {
    let mut inheritance = match previous {
        Some(binding) => binding,
        None => {
            if *selected != Selection::next(parent) && *selected != Selection::current(parent) {
                return None;
            }
            Inheritance {
                parent: parent.enrollment.address.clone(),
                applied: selected.clone(),
                pending: None,
            }
        }
    };
    if inheritance.pending.as_ref() == Some(selected) {
        inheritance.applied = selected.clone();
        inheritance.pending = None;
    } else if *selected != inheritance.applied {
        return None;
    }
    Some(inheritance)
}

#[cfg(test)]
#[path = "model_policy_tests.rs"]
mod tests;
