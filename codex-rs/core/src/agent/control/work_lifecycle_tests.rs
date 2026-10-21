//! Live lifecycle ownership across durable reconciliation and cancellation.

use super::*;
use ::test_case::test_case;
use pretty_assertions::assert_eq;

// Inject the store before startup so acceptance and the session's history receipts
// use the same tree controller. manager.agent_control() alone creates a new tree.
async fn start_mail_thread(
    manager: &ThreadManager,
    config: crate::config::Config,
    store: Arc<dyn AgentMailboxStore>,
) -> (
    crate::thread_manager::NewThread,
    super::super::LocalAgentControl,
) {
    let control = manager.agent_control();
    assert!(control.runtime.mail_store.set(store).is_ok());
    let target = control
        .runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            crate::StartThreadOptions::new(config),
            control.clone(),
            /*parent_thread_id*/ None,
            /*inherited_exec_policy*/ None,
        )
        .await
        .unwrap();
    let control = control.runtime.control(target.thread_id.into());
    assert_eq!(
        control.identity(),
        target.thread.session.services.agent_control.identity()
    );
    assert!(Arc::ptr_eq(
        &control.runtime.mail_store,
        &target
            .thread
            .session
            .services
            .local_agent_runtime
            .mail_store
    ));
    (target, control)
}

#[derive(Default)]
struct DeliveryEvents(
    std::sync::Mutex<Vec<(String, ThreadId)>>,
    tokio::sync::Notify,
);

impl codex_extension_api::MessageLifecycleContributor for DeliveryEvents {
    fn on_consumed(&self, input: &codex_extension_api::MessageLifecycleInput, recipient: ThreadId) {
        self.0
            .lock()
            .unwrap()
            .push((input.message_id.to_string(), recipient));
        self.1.notify_one();
    }
}

#[tokio::test]
async fn reconciled_live_mail_consumes_once_before_delayed_delivery_and_keeps_wake_debt() {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .unwrap();
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let store: Arc<dyn AgentMailboxStore> = Arc::new(InMemoryAgentMailboxStore::default());
    let (target, control) = start_mail_thread(&manager, config, Arc::clone(&store)).await;
    let root = control.session_id.into();
    let id = ResponseItemId::new("agentmail");
    let events = Arc::new(DeliveryEvents::default());
    let observers: Vec<Arc<dyn codex_extension_api::MessageLifecycleContributor>> =
        vec![events.clone()];
    let delivery = MessageDelivery::publish(root, id.clone(), &observers);
    let mut mail = InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::root(),
        Vec::new(),
        "reconcile before delivery".into(),
        /*trigger_turn*/ false,
    );
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: None,
        kind: AgentMailKind::DirectMessage,
        wake: AgentMailWake::Auto,
    });
    let (_, original) = control
        .accept_mail(
            target.thread_id,
            PendingAgentMessage::new(mail, Some(delivery)),
            &AgentCommunicationContext::new(
                crate::agent_communication::AgentCommunicationKind::Message,
                root,
            ),
        )
        .await
        .unwrap();

    // Read through the durable inbox before the original submission Op can arrive.
    let recovered = control.pending_mail(target.thread_id).await.unwrap();
    assert_eq!(recovered.len(), 1);
    let turn = target.thread.session.new_default_turn().await;
    target
        .thread
        .session
        .record_inter_agent_communication(&turn, turn.model_info(), recovered[0].clone())
        .await;
    assert_eq!(
        *events.0.lock().unwrap(),
        vec![(id.to_string(), target.thread_id)]
    );
    let recorded = store.pending(root, target.thread_id).await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].recorded_turn_id.as_deref(),
        Some(turn.sub_id.as_str())
    );
    assert_eq!(recorded[0].submitted_turn_id, None);
    assert_eq!(
        target
            .thread
            .session
            .input_queue
            .unsubmitted_recorded_mail()
            .await
            .len(),
        1
    );

    // The delayed route and later receipt repair share the already consumed handle.
    target
        .thread
        .session
        .record_inter_agent_communication(&turn, turn.model_info(), original)
        .await;
    assert_eq!(
        *events.0.lock().unwrap(),
        vec![(id.to_string(), target.thread_id)]
    );
    let history = target.thread.session.clone_history().await;
    let items: Vec<_> = history.raw_items().filter(|item| matches!(item,
        codex_protocol::models::ResponseItem::AgentMessage { id: Some(message_id), .. } if message_id == &id
    )).cloned().collect();
    assert_eq!(items.len(), 1);

    // Recording a callback is independent of the server accepting the selected prompt.
    control
        .mail_submitted(target.thread_id, &turn.sub_id, &items)
        .await
        .unwrap();
    target
        .thread
        .session
        .input_queue
        .accept_submitted_mail(&items)
        .await;
    assert!(
        control
            .pending_mail(target.thread_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        target
            .thread
            .session
            .input_queue
            .unsubmitted_recorded_mail()
            .await
            .is_empty()
    );
    target.thread.shutdown_and_wait().await.unwrap();
}

#[tokio::test]
async fn canceled_acceptance_keeps_live_delivery_until_durable_reconciliation() {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .unwrap();
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let acceptance = Arc::new(IndependentSubmission::default());
    let store = Arc::new(PausedSnapshotStore {
        inner: InMemoryAgentMailboxStore::default(),
        change: SnapshotChange::SubmitMail,
        pause_next: AtomicBool::new(false),
        read_started: tokio::sync::Notify::new(),
        continue_read: tokio::sync::Notify::new(),
        submission: None,
        attempt: None,
        acceptance: Some(Arc::clone(&acceptance)),
        settlement: None,
    });
    let erased_store: Arc<dyn AgentMailboxStore> = store.clone();
    let (target, control) = start_mail_thread(&manager, config, erased_store).await;
    let root = control.session_id.into();
    let recipient = target.thread_id;
    let id = ResponseItemId::new("agentmail");
    let events = Arc::new(DeliveryEvents::default());
    let observers: Vec<Arc<dyn codex_extension_api::MessageLifecycleContributor>> =
        vec![events.clone()];
    let delivery = MessageDelivery::publish(root, id.clone(), &observers);
    let mut mail = InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::root(),
        Vec::new(),
        "accepted after cancellation".into(),
        /*trigger_turn*/ false,
    );
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: None,
        kind: AgentMailKind::DirectMessage,
        wake: AgentMailWake::Auto,
    });
    let mut updates = control.watch_agent_work();
    updates.borrow_and_update();
    let caller_control = control.clone();
    let caller = tokio::spawn(async move {
        caller_control
            .accept_mail(
                recipient,
                PendingAgentMessage::new(mail, Some(delivery)),
                &AgentCommunicationContext::new(
                    crate::agent_communication::AgentCommunicationKind::Message,
                    root,
                ),
            )
            .await
    });
    acceptance.started.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(
        store
            .inner
            .pending(root, recipient)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(events.0.lock().unwrap().is_empty());
    acceptance.allow_commit.notify_one();
    updates.changed().await.unwrap();

    let pending = control.pending_mail(recipient).await.unwrap();
    assert_eq!(pending.len(), 1);
    let turn = target.thread.session.new_default_turn().await;
    target
        .thread
        .session
        .record_inter_agent_communication(&turn, turn.model_info(), pending[0].clone())
        .await;
    assert_eq!(*events.0.lock().unwrap(), vec![(id.to_string(), recipient)]);

    // Persisted payloads restore without publication state, even when history repair is needed.
    let serialized = serde_json::to_string(&pending[0]).unwrap();
    let restored: PendingAgentMessage = serde_json::from_str(&serialized).unwrap();
    assert!(restored.delivery.is_none());
    target
        .thread
        .session
        .record_inter_agent_communication(&turn, turn.model_info(), restored)
        .await;
    assert_eq!(*events.0.lock().unwrap(), vec![(id.to_string(), recipient)]);
    target.thread.shutdown_and_wait().await.unwrap();
}

#[derive(Clone, Copy)]
enum SettlementCaller {
    Waiting,
    Canceled,
}

#[test_case(SettlementCaller::Waiting; "waiting_caller")]
#[test_case(SettlementCaller::Canceled; "canceled_caller")]
#[tokio::test]
async fn completion_reconciliation_consumes_shared_handle_before_settlement_returns(
    caller_state: SettlementCaller,
) {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .unwrap();
    let events = Arc::new(DeliveryEvents::default());
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.message_lifecycle_contributor(events.clone());
    let auth = codex_login::AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth.clone(),
        crate::thread_manager::build_models_manager(&config, auth),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(extensions.build()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::passthrough_image_store(),
        Arc::new(codex_thread_store::InMemoryThreadStore::default()),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let settlement = Arc::new(IndependentSubmission::default());
    let store = Arc::new(PausedSnapshotStore {
        inner: InMemoryAgentMailboxStore::default(),
        change: SnapshotChange::SubmitMail,
        pause_next: AtomicBool::new(false),
        read_started: tokio::sync::Notify::new(),
        continue_read: tokio::sync::Notify::new(),
        submission: None,
        attempt: None,
        acceptance: None,
        settlement: Some(Arc::clone(&settlement)),
    });
    let erased: Arc<dyn AgentMailboxStore> = store.clone();
    let (parent, control) = start_mail_thread(&manager, config.clone(), erased).await;
    let child = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await
        .unwrap();
    parent.thread.session.mark_interrupted();
    let root = control.session_id.into();
    store
        .inner
        .put_task(AgentTaskRecord {
            task_id: "finishing-task".into(),
            root_id: root,
            owner_id: parent.thread_id,
            target_id: child.thread_id,
            name: Some("child".into()),
            status: AgentTaskStatus::Running,
            turn_id: Some("child-turn".into()),
            result_ref: None,
        })
        .await
        .unwrap();
    let outcome = AgentTurnOutcome {
        thread_id: child.thread_id,
        turn_id: "child-turn".into(),
        source: SessionSource::SubAgent(SubAgentSource::Other("child".into())),
        parent_turn_id: None,
        initiating_agent_path: None,
        status: AgentStatus::Completed(Some("settled result".into())),
        error_info: None,
    };
    let settlement_control = control.clone();
    let caller =
        tokio::spawn(async move { settlement_control.settle_consumed_tasks(&outcome).await });
    settlement.started.notified().await;
    let caller = match caller_state {
        SettlementCaller::Waiting => Some(caller),
        SettlementCaller::Canceled => {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            None
        }
    };
    // Task status and result are visible, while their best-effort chat route is still blocked.
    let pending = control.pending_mail(parent.thread_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    let id = pending[0].mail.as_ref().unwrap().message_id.clone();
    let turn = parent.thread.session.new_default_turn().await;
    let session = Arc::clone(&parent.thread.session);
    let recording = tokio::spawn(async move {
        session
            .record_inter_agent_communication(&turn, turn.model_info(), pending[0].clone())
            .await;
    });
    events.1.notified().await;
    assert_eq!(
        *events.0.lock().unwrap(),
        vec![(id.clone(), parent.thread_id)]
    );
    settlement.allow_commit.notify_one();
    recording.await.unwrap();
    if let Some(caller) = caller {
        caller.await.unwrap().unwrap();
    }
    parent.thread.shutdown_and_wait().await.unwrap();
    child.thread.shutdown_and_wait().await.unwrap();
    assert_eq!(*events.0.lock().unwrap(), vec![(id, parent.thread_id)]);
}

struct WorkGuardProbe {
    access: Arc<tokio::sync::RwLock<()>>,
    released: Arc<AtomicBool>,
    dropped: Arc<tokio::sync::Notify>,
}

impl Drop for WorkGuardProbe {
    fn drop(&mut self) {
        self.released
            .store(self.access.try_write().is_ok(), Ordering::SeqCst);
        self.dropped.notify_one();
    }
}

struct WorkGuardObserver {
    access: Arc<tokio::sync::RwLock<()>>,
    released: Arc<AtomicBool>,
    dropped: Arc<tokio::sync::Notify>,
}

impl codex_extension_api::MessageLifecycleContributor for WorkGuardObserver {
    fn on_publish(&self, input: &codex_extension_api::MessageLifecycleInput) {
        input.data.insert(WorkGuardProbe {
            access: Arc::clone(&self.access),
            released: Arc::clone(&self.released),
            dropped: Arc::clone(&self.dropped),
        });
    }
}

#[tokio::test]
async fn canceled_work_mutation_releases_publication_after_store_guard() {
    let config = test_config().await;
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider,
    );
    let control = manager.agent_control();
    let access = Arc::clone(&control.runtime.work_store_access);
    let released = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(tokio::sync::Notify::new());
    let observer: Arc<dyn codex_extension_api::MessageLifecycleContributor> =
        Arc::new(WorkGuardObserver {
            access: Arc::clone(&access),
            released: Arc::clone(&released),
            dropped: Arc::clone(&dropped),
        });
    let id = ResponseItemId::new("agentmail");
    let delivery = MessageDelivery::publish(control.session_id.into(), id, &[observer]);
    let pending = PendingAgentMessage::new(
        InterAgentCommunication::new(
            codex_protocol::AgentPath::root(),
            codex_protocol::AgentPath::root(),
            Vec::new(),
            "retained during commit".into(),
            /*trigger_turn*/ false,
        ),
        Some(delivery),
    );
    let gate = Arc::new(IndependentSubmission::default());
    let mutation_gate = Arc::clone(&gate);
    let caller = tokio::spawn(async move {
        control
            .mutate_work_retaining(Some(pending), async move {
                mutation_gate.started.notify_one();
                mutation_gate.allow_commit.notified().await;
                Ok(())
            })
            .await
    });
    gate.started.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(
        access.try_write().is_err(),
        "detached mutation still owns its guard"
    );
    gate.allow_commit.notify_one();
    dropped.notified().await;
    assert!(
        released.load(Ordering::SeqCst),
        "publication attachment dropped before storage unlocked"
    );
}
