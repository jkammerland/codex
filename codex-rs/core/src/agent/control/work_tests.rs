use super::*;
use crate::ThreadManager;
use crate::agent::api::AgentControl;
use crate::agent::api::AgentTurnOutcome;
use crate::config::test_config;
use codex_login::CodexAuth;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::AgentMailAcceptance;
use codex_thread_store::AgentMailRecord;
use codex_thread_store::AgentMailboxSummary;
use codex_thread_store::AgentTaskSnapshot;
use codex_thread_store::InMemoryAgentMailboxStore;
use codex_thread_store::ThreadStoreFuture;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;
use test_case::test_case;

#[test_case(AgentStatus::Errored("transport refused".into()), None, AgentTaskStatus::Failed; "failed before response created")]
#[test_case(AgentStatus::Interrupted, None, AgentTaskStatus::Held; "interrupted before response created")]
#[test_case(AgentStatus::Errored("denied".into()), Some(CodexErrorInfo::TooManyDenials), AgentTaskStatus::Held; "safety stop before response created")]
#[test_case(AgentStatus::Completed(Some("unaccepted output".into())), None, AgentTaskStatus::Queued; "no success without response created")]
#[tokio::test]
async fn attempted_assignments_settle_without_consuming_unsubmitted_or_later_mail(
    status: AgentStatus,
    error_info: Option<CodexErrorInfo>,
    expected_status: AgentTaskStatus,
) {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .unwrap();
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let target = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await
        .unwrap();
    let control = manager.agent_control();
    let store: Arc<dyn AgentMailboxStore> = Arc::new(InMemoryAgentMailboxStore::default());
    assert!(control.runtime.mail_store.set(Arc::clone(&store)).is_ok());
    let root_id = control.session_id.into();
    let target_id = target.thread_id;
    let task = AgentTaskRecord {
        task_id: "attempted".into(),
        root_id,
        owner_id: root_id,
        target_id,
        name: Some("worker".into()),
        status: AgentTaskStatus::Queued,
        turn_id: None,
        result_ref: None,
    };
    let envelope = AgentMailEnvelope {
        message_id: "request".into(),
        root_id,
        sender_id: root_id,
        recipient_id: target_id,
        kind: "assignment".into(),
        payload_json: "{}".into(),
        wake: true,
        task_id: Some(task.task_id.clone()),
    };
    store
        .accept_task(task.clone(), envelope.clone())
        .await
        .unwrap();
    store
        .mark_recorded(
            root_id,
            target_id,
            vec![envelope.message_id.clone()],
            "attempt".into(),
        )
        .await
        .unwrap();
    store
        .mark_attempted(
            root_id,
            target_id,
            vec![envelope.message_id.clone()],
            "attempt".into(),
        )
        .await
        .unwrap();

    // A task accepted after the selected request must not inherit its failure.
    let mut later = task.clone();
    later.task_id = "later".into();
    let mut later_mail = envelope.clone();
    later_mail.message_id = "later-request".into();
    later_mail.task_id = Some(later.task_id.clone());
    store.accept_task(later.clone(), later_mail).await.unwrap();
    let pending_before = store.pending(root_id, target_id).await.unwrap();
    assert!(
        pending_before
            .iter()
            .all(|record| record.submitted_turn_id.is_none())
    );

    control
        .settle_consumed_tasks(&AgentTurnOutcome {
            thread_id: target_id,
            turn_id: "attempt".into(),
            source: SessionSource::SubAgent(SubAgentSource::Other("worker".into())),
            parent_turn_id: None,
            initiating_agent_path: None,
            status,
            error_info,
        })
        .await
        .unwrap();
    let completions = store.pending(root_id, root_id).await.unwrap();
    let mut expected = task;
    expected.status = expected_status;
    expected.turn_id = Some("attempt".into());
    if expected_status == AgentTaskStatus::Failed {
        assert_eq!(completions.len(), 1);
        assert_eq!(
            completions[0].envelope.task_id,
            Some(expected.task_id.clone())
        );
        expected.result_ref = Some(completions[0].envelope.message_id.clone());
    } else {
        assert_eq!(completions, vec![]);
    }
    assert_eq!(
        store
            .tasks(root_id, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![expected, later]
    );
    assert_eq!(
        store.pending(root_id, target_id).await.unwrap(),
        pending_before
    );
    target.thread.shutdown_and_wait().await.unwrap();
}

#[derive(Clone, Copy)]
enum SnapshotChange {
    SubmitMail,
    ClearBarrier,
}

struct PausedSnapshotStore {
    inner: InMemoryAgentMailboxStore,
    change: SnapshotChange,
    pause_next: AtomicBool,
    read_started: tokio::sync::Notify,
    continue_read: tokio::sync::Notify,
    submission: Option<Arc<IndependentSubmission>>,
    attempt: Option<Arc<IndependentSubmission>>,
    acceptance: Option<Arc<IndependentSubmission>>,
    settlement: Option<Arc<IndependentSubmission>>,
}

#[derive(Default)]
struct IndependentSubmission {
    started: tokio::sync::Notify,
    allow_commit: tokio::sync::Notify,
    committed: tokio::sync::Notify,
}

impl PausedSnapshotStore {
    async fn pause_read(&self) {
        if self.pause_next.swap(false, Ordering::SeqCst) {
            self.read_started.notify_one();
            self.continue_read.notified().await;
        }
    }
}

impl AgentMailboxStore for PausedSnapshotStore {
    fn accept(&self, envelope: AgentMailEnvelope) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        let Some(acceptance) = self.acceptance.as_ref().map(Arc::clone) else {
            return self.inner.accept(envelope);
        };
        Box::pin(async move {
            acceptance.started.notify_one();
            acceptance.allow_commit.notified().await;
            let result = self.inner.accept(envelope).await;
            acceptance.committed.notify_one();
            result
        })
    }

    fn pending(
        &self,
        root: ThreadId,
        recipient: ThreadId,
    ) -> ThreadStoreFuture<'_, Vec<AgentMailRecord>> {
        Box::pin(async move {
            let records = self.inner.pending(root, recipient).await?;
            if matches!(self.change, SnapshotChange::SubmitMail) {
                self.pause_read().await;
            }
            Ok(records)
        })
    }

    fn accepted_ids(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: Vec<String>,
    ) -> ThreadStoreFuture<'_, Vec<String>> {
        self.inner.accepted_ids(root, recipient, ids)
    }

    fn mark_attempted(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: Vec<String>,
        turn: String,
    ) -> ThreadStoreFuture<'_, ()> {
        let Some(attempt) = self.attempt.as_ref().map(Arc::clone) else {
            return self.inner.mark_attempted(root, recipient, ids, turn);
        };
        let inner = self.inner.clone();
        let worker = tokio::spawn(async move {
            attempt.started.notify_one();
            attempt.allow_commit.notified().await;
            let result = inner.mark_attempted(root, recipient, ids, turn).await;
            attempt.committed.notify_one();
            result
        });
        Box::pin(async move { worker.await.expect("join independent attempt worker") })
    }

    fn mark_recorded(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: Vec<String>,
        turn: String,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.mark_recorded(root, recipient, ids, turn)
    }

    fn mark_submitted(
        &self,
        root: ThreadId,
        recipient: ThreadId,
        ids: Vec<String>,
        turn: String,
    ) -> ThreadStoreFuture<'_, ()> {
        let Some(submission) = self.submission.as_ref().map(Arc::clone) else {
            return self.inner.mark_submitted(root, recipient, ids, turn);
        };
        let inner = self.inner.clone();
        // Model SQLite's independent worker: dropping the waiting future does
        // not cancel a queued commit or roll back its successful completion.
        let worker = tokio::spawn(async move {
            submission.started.notify_one();
            submission.allow_commit.notified().await;
            let result = inner.mark_submitted(root, recipient, ids, turn).await;
            submission.committed.notify_one();
            result
        });
        Box::pin(async move { worker.await.expect("join independent storage worker") })
    }

    fn put_task(&self, task: AgentTaskRecord) -> ThreadStoreFuture<'_, ()> {
        self.inner.put_task(task)
    }

    fn accept_task(
        &self,
        task: AgentTaskRecord,
        request: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        self.inner.accept_task(task, request)
    }

    fn settle_task(
        &self,
        task: AgentTaskRecord,
        completion: AgentMailEnvelope,
    ) -> ThreadStoreFuture<'_, AgentMailAcceptance> {
        let Some(settlement) = self.settlement.as_ref().map(Arc::clone) else {
            return self.inner.settle_task(task, completion);
        };
        Box::pin(async move {
            let result = self.inner.settle_task(task, completion).await;
            settlement.started.notify_one();
            settlement.allow_commit.notified().await;
            result
        })
    }

    fn tasks(
        &self,
        root: ThreadId,
        filter: AgentTaskFilter,
    ) -> ThreadStoreFuture<'_, AgentTaskSnapshot> {
        Box::pin(async move {
            let barrier_read = !filter.task_ids.is_empty();
            let snapshot = self.inner.tasks(root, filter).await?;
            if barrier_read && matches!(self.change, SnapshotChange::ClearBarrier) {
                self.pause_read().await;
            }
            Ok(snapshot)
        })
    }

    fn summary(&self, root: ThreadId) -> ThreadStoreFuture<'_, AgentMailboxSummary> {
        self.inner.summary(root)
    }

    fn remove_root(&self, root: ThreadId) -> ThreadStoreFuture<'_, ()> {
        self.inner.remove_root(root)
    }
}

#[test_case(SnapshotChange::SubmitMail; "submission_during_pending_read")]
#[test_case(SnapshotChange::ClearBarrier; "barrier_change_during_task_read")]
#[tokio::test]
async fn work_summary_keeps_counts_and_revision_coherent(change: SnapshotChange) {
    let config = test_config().await;
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider,
    );
    let control = manager.agent_control();
    let store = Arc::new(PausedSnapshotStore {
        inner: InMemoryAgentMailboxStore::default(),
        change,
        pause_next: AtomicBool::new(false),
        read_started: tokio::sync::Notify::new(),
        continue_read: tokio::sync::Notify::new(),
        submission: None,
        attempt: None,
        acceptance: None,
        settlement: None,
    });
    let erased_store: Arc<dyn AgentMailboxStore> = store.clone();
    assert!(control.runtime.mail_store.set(erased_store).is_ok());

    let root = control.session_id.into();
    let agent = ThreadId::new();
    let id = codex_protocol::ResponseItemId::new("agentmail");
    let mut mail = InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::root()
            .join("worker")
            .expect("worker path"),
        Vec::new(),
        "held assignment".into(),
        /*trigger_turn*/ true,
    );
    mail.id = Some(id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: Some("assignment".into()),
        kind: AgentMailKind::Assignment,
        wake: AgentMailWake::Auto,
    });
    store
        .inner
        .accept_task(
            AgentTaskRecord {
                task_id: "assignment".into(),
                root_id: root,
                owner_id: root,
                target_id: agent,
                name: Some("worker".into()),
                status: AgentTaskStatus::Held,
                turn_id: None,
                result_ref: None,
            },
            AgentMailEnvelope {
                message_id: id.to_string(),
                root_id: root,
                sender_id: root,
                recipient_id: agent,
                kind: serde_json::to_string(&AgentMailKind::Assignment).expect("assignment kind"),
                payload_json: serde_json::to_string(&mail).expect("assignment payload"),
                wake: true,
                task_id: Some("assignment".into()),
            },
        )
        .await
        .expect("accept held assignment");
    store
        .inner
        .mark_recorded(root, agent, vec![id.to_string()], "recorded".into())
        .await
        .expect("record assignment");
    control.set_task_barrier(agent, Some(vec!["assignment".into()]));
    let updates = control.watch_agent_work();
    let prior = AgentWorkSummary {
        revision: *updates.borrow(),
        pending_mail: 1,
        waiting_tasks: Some(AgentBarrierSummary {
            total: 1,
            remaining: 1,
            held: 1,
            unavailable: 0,
        }),
    };

    store.pause_next.store(true, Ordering::SeqCst);
    let snapshot_control = control.clone();
    let snapshot = tokio::spawn(async move { snapshot_control.work_summary(agent).await });
    store.read_started.notified().await;

    // Drive the mutation at the exact point where old counts have been read.
    // A submission must wait for that snapshot; a synchronous barrier update can
    // complete immediately, but cannot change the snapshot's captured revision.
    let items = vec![mail.to_model_input_item()];
    let mut submission = control.mail_submitted(agent, "submitted", &items);
    let progress = match change {
        SnapshotChange::SubmitMail => Some(futures::poll!(submission.as_mut())),
        SnapshotChange::ClearBarrier => {
            control.set_task_barrier(agent, None);
            None
        }
    };
    store.continue_read.notify_one();
    let captured = snapshot
        .await
        .expect("join snapshot read")
        .expect("read snapshot");
    match progress {
        Some(Poll::Ready(result)) => result.expect("submit assignment"),
        Some(Poll::Pending) => submission.await.expect("submit assignment"),
        None => {}
    }
    assert_eq!(captured, prior);

    let current = AgentWorkSummary {
        revision: prior.revision + 1,
        pending_mail: match change {
            SnapshotChange::SubmitMail => 0,
            SnapshotChange::ClearBarrier => 1,
        },
        waiting_tasks: match change {
            SnapshotChange::SubmitMail => Some(AgentBarrierSummary {
                total: 1,
                remaining: 1,
                held: 0,
                unavailable: 0,
            }),
            SnapshotChange::ClearBarrier => None,
        },
    };
    assert_eq!(
        control
            .work_summary(agent)
            .await
            .expect("read current snapshot"),
        current
    );
    assert_eq!(*updates.borrow(), current.revision);
}

#[tokio::test]
async fn canceled_work_mutation_publishes_only_after_independent_commit() {
    let config = test_config().await;
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider,
    );
    let control = manager.agent_control();
    let submission = Arc::new(IndependentSubmission::default());
    let store = Arc::new(PausedSnapshotStore {
        inner: InMemoryAgentMailboxStore::default(),
        change: SnapshotChange::SubmitMail,
        pause_next: AtomicBool::new(false),
        read_started: tokio::sync::Notify::new(),
        continue_read: tokio::sync::Notify::new(),
        submission: Some(Arc::clone(&submission)),
        attempt: None,
        acceptance: None,
        settlement: None,
    });
    let erased_store: Arc<dyn AgentMailboxStore> = store.clone();
    assert!(control.runtime.mail_store.set(erased_store).is_ok());

    let root = control.session_id.into();
    let agent = ThreadId::new();
    let id = codex_protocol::ResponseItemId::new("agentmail");
    let mut mail = InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::root()
            .join("worker")
            .expect("worker path"),
        Vec::new(),
        "accepted message".into(),
        /*trigger_turn*/ false,
    );
    mail.id = Some(id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: None,
        kind: AgentMailKind::DirectMessage,
        wake: AgentMailWake::Auto,
    });
    store
        .inner
        .accept(AgentMailEnvelope {
            message_id: id.to_string(),
            root_id: root,
            sender_id: root,
            recipient_id: agent,
            kind: serde_json::to_string(&AgentMailKind::DirectMessage)
                .expect("direct message kind"),
            payload_json: serde_json::to_string(&mail).expect("direct message payload"),
            wake: true,
            task_id: None,
        })
        .await
        .expect("accept direct message");
    store
        .inner
        .mark_recorded(root, agent, vec![id.to_string()], "recorded".into())
        .await
        .expect("record direct message");
    let mut updates = control.watch_agent_work();
    let prior_revision = *updates.borrow_and_update();

    let mutation_control = control.clone();
    let caller = tokio::spawn(async move {
        mutation_control
            .mail_submitted(agent, "submitted", &[mail.to_model_input_item()])
            .await
    });
    submission.started.notified().await;
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("caller was canceled")
            .is_cancelled()
    );

    // Cancellation has not committed storage or published its final revision.
    // A concurrent snapshot must stay blocked, rather than stamp old counts
    // with the revision that will belong to the eventual commit.
    assert!(!updates.has_changed().expect("work observer remains open"));
    assert_eq!(
        store
            .inner
            .pending(root, agent)
            .await
            .expect("read pre-commit mail")
            .len(),
        1
    );
    let mut snapshot = Box::pin(control.work_summary(agent));
    assert!(futures::poll!(snapshot.as_mut()).is_pending());

    submission.allow_commit.notify_one();
    submission.committed.notified().await;
    updates
        .changed()
        .await
        .expect("completed mutation publishes revision");
    let expected = AgentWorkSummary {
        revision: prior_revision + 1,
        pending_mail: 0,
        waiting_tasks: None,
    };
    assert_eq!(snapshot.await.expect("read committed snapshot"), expected);
    assert_eq!(*updates.borrow(), expected.revision);
    assert_eq!(
        store
            .inner
            .pending(root, agent)
            .await
            .expect("read committed mail"),
        Vec::new()
    );
}

#[tokio::test]
async fn interrupted_settlement_waits_for_canceled_assignment_attempt() {
    let mut config = test_config().await;
    config
        .features
        .enable(codex_features::Feature::WakeOnAgentMessages)
        .expect("enable agent mail");
    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let target = manager
        .start_thread(crate::StartThreadOptions::new(config))
        .await
        .expect("start recipient");
    let control = manager.agent_control();
    let attempt = Arc::new(IndependentSubmission::default());
    let store = Arc::new(PausedSnapshotStore {
        inner: InMemoryAgentMailboxStore::default(),
        change: SnapshotChange::SubmitMail,
        pause_next: AtomicBool::new(false),
        read_started: tokio::sync::Notify::new(),
        continue_read: tokio::sync::Notify::new(),
        submission: None,
        attempt: Some(Arc::clone(&attempt)),
        acceptance: None,
        settlement: None,
    });
    let erased_store: Arc<dyn AgentMailboxStore> = store.clone();
    assert!(control.runtime.mail_store.set(erased_store).is_ok());

    let root = control.session_id.into();
    let agent = target.thread_id;
    let id = codex_protocol::ResponseItemId::new("agentmail");
    let mut mail = InterAgentCommunication::new(
        codex_protocol::AgentPath::root(),
        codex_protocol::AgentPath::root()
            .join("worker")
            .expect("worker path"),
        Vec::new(),
        "attempt this assignment".into(),
        /*trigger_turn*/ true,
    );
    mail.id = Some(id.clone());
    mail.mail = Some(AgentMailMetadata {
        message_id: id.to_string(),
        task_id: Some("attempted".into()),
        kind: AgentMailKind::Assignment,
        wake: AgentMailWake::Auto,
    });
    let task = AgentTaskRecord {
        task_id: "attempted".into(),
        root_id: root,
        owner_id: root,
        target_id: agent,
        name: Some("worker".into()),
        status: AgentTaskStatus::Queued,
        turn_id: None,
        result_ref: None,
    };
    let envelope = AgentMailEnvelope {
        message_id: id.to_string(),
        root_id: root,
        sender_id: root,
        recipient_id: agent,
        kind: serde_json::to_string(&AgentMailKind::Assignment).expect("assignment kind"),
        payload_json: serde_json::to_string(&mail).expect("assignment payload"),
        wake: true,
        task_id: Some(task.task_id.clone()),
    };
    store
        .inner
        .accept_task(task.clone(), envelope.clone())
        .await
        .expect("accept attempted assignment");
    let mut later = task.clone();
    later.task_id = "later".into();
    let mut later_mail = envelope;
    later_mail.message_id = "later-message".into();
    later_mail.task_id = Some(later.task_id.clone());
    store
        .inner
        .accept_task(later.clone(), later_mail)
        .await
        .expect("accept unselected assignment");
    let pending = store
        .inner
        .pending(root, agent)
        .await
        .expect("read accepted mail");

    let attempt_control = control.clone();
    let caller = tokio::spawn(async move {
        attempt_control
            .mail_attempted(agent, "interrupted-turn", &[mail.to_model_input_item()])
            .await
    });
    attempt.started.notified().await;
    caller.abort();
    assert!(
        caller
            .await
            .expect_err("attempt caller was canceled")
            .is_cancelled()
    );

    let outcome = AgentTurnOutcome {
        thread_id: agent,
        turn_id: "interrupted-turn".into(),
        source: SessionSource::SubAgent(SubAgentSource::Other("worker".into())),
        parent_turn_id: None,
        initiating_agent_path: None,
        status: AgentStatus::Interrupted,
        error_info: None,
    };
    let mut settlement = Box::pin(control.settle_consumed_tasks(&outcome));
    assert!(futures::poll!(settlement.as_mut()).is_pending());
    assert_eq!(
        store
            .inner
            .tasks(root, AgentTaskFilter::default())
            .await
            .expect("read before commit")
            .tasks,
        vec![task.clone(), later.clone()]
    );

    attempt.allow_commit.notify_one();
    attempt.committed.notified().await;
    assert!(!settlement.await.expect("settle interrupted attempt"));
    let mut expected = task;
    expected.status = AgentTaskStatus::Held;
    expected.turn_id = Some("interrupted-turn".into());
    assert_eq!(
        store
            .inner
            .tasks(root, AgentTaskFilter::default())
            .await
            .expect("read settled tasks")
            .tasks,
        vec![expected, later]
    );
    assert_eq!(
        store
            .inner
            .pending(root, agent)
            .await
            .expect("read retained receipts"),
        pending
    );
    assert_eq!(
        store
            .inner
            .pending(root, root)
            .await
            .expect("read completion mail"),
        Vec::new()
    );
    target
        .thread
        .shutdown_and_wait()
        .await
        .expect("stop recipient");
}

#[path = "work_lifecycle_tests.rs"]
mod lifecycle;
