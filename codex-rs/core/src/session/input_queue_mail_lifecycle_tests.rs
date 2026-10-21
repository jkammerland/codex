//! Queue rejection and pruning must release extension attachments after unlocking.

use super::*;
use crate::agent_communication::MessageDelivery;
use codex_extension_api::MessageLifecycleContributor;
use codex_extension_api::MessageLifecycleInput;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::protocol::AgentMailKind;
use codex_protocol::protocol::AgentMailMetadata;
use codex_protocol::protocol::AgentMailWake;
use pretty_assertions::assert_eq;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct UnlockProbe {
    queue: Weak<InputQueue>,
    dropped: Arc<AtomicUsize>,
}

impl Drop for UnlockProbe {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.upgrade() {
            assert!(
                queue.mailbox.try_lock().is_ok(),
                "attachment dropped under mailbox lock"
            );
        }
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct Publication {
    queue: Weak<InputQueue>,
    dropped: Arc<AtomicUsize>,
}

impl MessageLifecycleContributor for Publication {
    fn on_publish(&self, input: &MessageLifecycleInput) {
        input.data.insert(UnlockProbe {
            queue: self.queue.clone(),
            dropped: Arc::clone(&self.dropped),
        });
    }
}

#[derive(Clone, Copy)]
enum Removal {
    Duplicate,
    Submitted,
    Receipt,
    Settled,
}

#[test_case::test_case(Removal::Duplicate; "duplicate")]
#[test_case::test_case(Removal::Submitted; "submitted")]
#[test_case::test_case(Removal::Receipt; "receipt_prunes_queued_delivery")]
#[test_case::test_case(Removal::Settled; "settled")]
#[tokio::test]
async fn mailbox_releases_attachments_outside_lock(removal: Removal) {
    let queue = Arc::new(InputQueue::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn MessageLifecycleContributor> = Arc::new(Publication {
        queue: Arc::downgrade(&queue),
        dropped: Arc::clone(&dropped),
    });
    let id = ResponseItemId::new("agentmail");
    let mut mail = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "queued".into(),
        /*trigger_turn*/ false,
    );
    mail.id = Some(id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: Some("assignment".into()),
        kind: AgentMailKind::Assignment,
        wake: AgentMailWake::Auto,
    });
    match removal {
        Removal::Duplicate => {
            assert!(
                queue
                    .enqueue_mailbox_communication(mail.clone().into(), Default::default())
                    .await
            );
        }
        Removal::Submitted => {
            queue.record_mail(&mail).await;
            queue
                .accept_submitted_mail(&[mail.to_model_input_item()])
                .await;
        }
        Removal::Receipt | Removal::Settled => {}
    }
    let receipt_item = mail.to_model_input_item();
    if matches!(removal, Removal::Receipt) {
        queue.record_mail(&mail).await;
    }
    let delivery = MessageDelivery::publish(ThreadId::new(), id, &[observer]);
    let inserted = queue
        .enqueue_mailbox_communication(
            crate::PendingAgentMessage::new(mail, Some(delivery)),
            Default::default(),
        )
        .await;
    assert_eq!(
        inserted,
        matches!(removal, Removal::Receipt | Removal::Settled)
    );
    if matches!(removal, Removal::Receipt) {
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        queue.accept_submitted_mail(&[receipt_item]).await;
        assert!(queue.drain_mailbox().await.is_empty());
    }
    if matches!(removal, Removal::Settled) {
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        queue
            .discard_settled_assignments(&HashSet::from(["assignment".into()]))
            .await;
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
