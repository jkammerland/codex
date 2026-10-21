use super::*;
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

struct Released {
    index: Weak<LiveMail>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Released {
    fn drop(&mut self) {
        if let Some(index) = self.index.upgrade() {
            assert!(
                index.0.try_lock().is_ok(),
                "publication dropped under index lock"
            );
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Observer {
    index: Weak<LiveMail>,
    drops: Arc<AtomicUsize>,
}

impl MessageLifecycleContributor for Observer {
    fn on_publish(&self, input: &MessageLifecycleInput) {
        input.data.insert(Released {
            index: self.index.clone(),
            drops: Arc::clone(&self.drops),
        });
    }
}

#[test_case::test_case(false; "rejected")]
#[test_case::test_case(true; "committed_until_shutdown")]
fn durable_registration_releases_attachments_without_holding_index_lock(committed: bool) {
    let index = Arc::new(LiveMail::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let id = ResponseItemId::new("agentmail");
    let recipient = ThreadId::new();
    let observer: Arc<dyn MessageLifecycleContributor> = Arc::new(Observer {
        index: Arc::downgrade(&index),
        drops: Arc::clone(&drops),
    });
    let mut raw = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "pending".into(),
        /*trigger_turn*/ false,
    );
    raw.id = Some(id.clone());
    raw.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: None,
        kind: AgentMailKind::DirectMessage,
        wake: AgentMailWake::Auto,
    });
    let pending = PendingAgentMessage::new(
        raw.clone(),
        Some(MessageDelivery::publish(ThreadId::new(), id, &[observer])),
    );
    let registration = index
        .register(recipient, &pending)
        .expect("new registration");
    drop(pending);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    if committed {
        registration.committed();
        assert!(index.attach(recipient, raw.clone()).delivery.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        index.close();
    } else {
        drop(registration);
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(index.attach(recipient, raw).delivery.is_none());
}
