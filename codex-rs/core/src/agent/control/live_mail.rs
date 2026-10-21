//! Shares live delivery ownership with durable mailbox reconciliation.
//! The index is process-local: restored serialized mail never republishes hooks.

use crate::PendingAgentMessage;
use crate::agent_communication::MessageDelivery;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

type MessageKey = (ThreadId, String);

pub(super) struct LiveMail(Mutex<Option<HashMap<MessageKey, MessageDelivery>>>);

impl LiveMail {
    pub(super) fn new() -> Self {
        Self(Mutex::new(Some(HashMap::new())))
    }

    /// Register before a durable mutation can expose the corresponding envelope.
    pub(super) fn register(
        self: &Arc<Self>,
        recipient: ThreadId,
        mail: &PendingAgentMessage,
    ) -> Option<Registration> {
        let metadata = mail.mail.as_ref()?;
        let delivery = mail.delivery.as_ref()?;
        let key = (recipient, metadata.message_id.clone());
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = entries.as_mut()?;
        if entries.contains_key(&key) || !delivery.is_pending() {
            return None;
        }
        entries.insert(key.clone(), delivery.clone());
        Some(Registration {
            index: Arc::clone(self),
            key: Some(key),
            delivery: delivery.clone(),
        })
    }

    pub(super) fn attach(
        &self,
        recipient: ThreadId,
        communication: InterAgentCommunication,
    ) -> PendingAgentMessage {
        let delivery = communication.mail.as_ref().and_then(|metadata| {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .and_then(|entries| entries.get(&(recipient, metadata.message_id.clone())))
                .cloned()
        });
        PendingAgentMessage::new(communication, delivery)
    }

    pub(super) fn remove(&self, recipient: ThreadId, message_id: &str) {
        let delivery = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|entries| entries.remove(&(recipient, message_id.to_owned())));
        // Publication attachments may reenter the runtime when dropped.
        drop(delivery);
    }

    pub(super) fn remove_recipient(&self, recipient: ThreadId) {
        let removed = {
            let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            entries.as_mut().map(|entries| {
                entries
                    .extract_if(|(agent, _), _| *agent == recipient)
                    .collect::<Vec<_>>()
            })
        };
        drop(removed);
    }

    pub(super) fn close(&self) {
        let entries = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(entries);
    }
}

/// Failed or cancelled pre-commit work must not leave orphaned live attachments.
/// The detached storage future owns this guard until acceptance is known.
pub(super) struct Registration {
    index: Arc<LiveMail>,
    key: Option<MessageKey>,
    delivery: MessageDelivery,
}

impl Registration {
    pub(super) fn committed(mut self) {
        self.key = None;
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some((recipient, message_id)) = &self.key {
            let removed = {
                let mut entries = self.index.0.lock().unwrap_or_else(PoisonError::into_inner);
                entries.as_mut().and_then(|entries| {
                    let key = (*recipient, message_id.clone());
                    if entries
                        .get(&key)
                        .is_some_and(|delivery| delivery.same_delivery(&self.delivery))
                    {
                        entries.remove(&key)
                    } else {
                        None
                    }
                })
            };
            drop(removed);
        }
    }
}

#[cfg(test)]
#[path = "live_mail_tests.rs"]
mod tests;
