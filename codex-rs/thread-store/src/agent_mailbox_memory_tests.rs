use super::*;
use pretty_assertions::assert_eq;

fn fixture() -> (AgentTaskRecord, AgentMailEnvelope) {
    let root_id = ThreadId::new();
    let target_id = ThreadId::new();
    let task = AgentTaskRecord {
        task_id: "task".into(),
        root_id,
        owner_id: root_id,
        target_id,
        name: None,
        status: AgentTaskStatus::Queued,
        turn_id: None,
        result_ref: None,
    };
    let request = AgentMailEnvelope {
        message_id: "request".into(),
        root_id,
        sender_id: root_id,
        recipient_id: target_id,
        kind: "task_request".into(),
        payload_json: "{}".into(),
        wake: true,
        task_id: Some(task.task_id.clone()),
    };
    (task, request)
}

#[tokio::test]
async fn ephemeral_store_preserves_receipts_and_atomic_task_lifecycle() {
    let store: Arc<dyn AgentMailboxStore> = Arc::new(InMemoryAgentMailboxStore::default());
    let (mut task, request) = fixture();
    let accepted = store
        .accept_task(task.clone(), request.clone())
        .await
        .unwrap();
    let initial = store.summary(task.root_id).await.unwrap();
    assert_eq!(
        store.accept(request.clone()).await.unwrap(),
        AgentMailAcceptance {
            record: accepted.record.clone(),
            inserted: false
        }
    );
    assert_eq!(store.summary(task.root_id).await.unwrap(), initial);

    assert!(
        store
            .mark_recorded(
                task.root_id,
                task.target_id,
                vec!["request".into(), "missing".into()],
                "turn".into()
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.pending(task.root_id, task.target_id).await.unwrap(),
        vec![accepted.record.clone()]
    );
    assert!(
        store
            .mark_submitted(
                task.root_id,
                task.target_id,
                vec!["request".into()],
                "turn".into()
            )
            .await
            .is_err()
    );
    store
        .mark_recorded(
            task.root_id,
            task.target_id,
            vec!["request".into()],
            "recorded".into(),
        )
        .await
        .unwrap();
    let mut recorded = accepted.record;
    recorded.recorded_turn_id = Some("recorded".into());
    assert_eq!(
        store.pending(task.root_id, task.target_id).await.unwrap(),
        vec![recorded]
    );
    store
        .mark_attempted(
            task.root_id,
            task.target_id,
            vec!["request".into()],
            "attempted".into(),
        )
        .await
        .unwrap();
    let mut attempted = task.clone();
    attempted.turn_id = Some("attempted".into());
    let snapshot = store
        .tasks(task.root_id, AgentTaskFilter::default())
        .await
        .unwrap();
    assert_eq!(snapshot.tasks, vec![attempted]);
    store
        .mark_attempted(
            task.root_id,
            task.target_id,
            vec!["request".into()],
            "attempted".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap(),
        snapshot
    );
    assert!(
        store
            .pending(task.root_id, task.target_id)
            .await
            .unwrap()
            .iter()
            .all(|record| record.submitted_turn_id.is_none())
    );
    store
        .mark_submitted(
            task.root_id,
            task.target_id,
            vec!["request".into()],
            "submitted".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending(task.root_id, task.target_id).await.unwrap(),
        vec![]
    );
    store
        .accept_task(task.clone(), request.clone())
        .await
        .unwrap();
    task.status = AgentTaskStatus::Running;
    task.turn_id = Some("submitted".into());
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![task.clone()]
    );

    let before = store
        .tasks(task.root_id, AgentTaskFilter::default())
        .await
        .unwrap();
    let mut reset = task.clone();
    reset.status = AgentTaskStatus::Queued;
    reset.turn_id = None;
    let mut second_request = request.clone();
    second_request.message_id = "second-request".into();
    assert!(store.accept_task(reset, second_request).await.is_err());
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );

    task.status = AgentTaskStatus::Held;
    store.put_task(task.clone()).await.unwrap();
    store
        .mark_submitted(
            task.root_id,
            task.target_id,
            vec!["request".into()],
            "resumed".into(),
        )
        .await
        .unwrap();
    task.status = AgentTaskStatus::Running;
    task.turn_id = Some("resumed".into());
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![task.clone()]
    );
    let receipt = store.accept(request.clone()).await.unwrap().record;
    assert_eq!(
        (receipt.recorded_turn_id, receipt.submitted_turn_id),
        (Some("recorded".into()), Some("submitted".into()))
    );

    assert_eq!(
        store
            .accepted_ids(
                task.root_id,
                task.target_id,
                vec!["request".into(), "unknown".into(), "request".into()]
            )
            .await
            .unwrap(),
        vec!["request".to_string()]
    );
    assert_eq!(
        store
            .accepted_ids(task.root_id, task.owner_id, vec!["request".into()])
            .await
            .unwrap(),
        Vec::<String>::new()
    );

    let mut completion = request;
    completion.message_id = "completion".into();
    completion.kind = "task_result".into();
    completion.sender_id = task.target_id;
    completion.recipient_id = task.owner_id;
    task.status = AgentTaskStatus::Completed;
    task.result_ref = Some("result".into());
    let completed = store
        .settle_task(task.clone(), completion.clone())
        .await
        .unwrap();
    assert_eq!(
        store.pending(task.root_id, task.owner_id).await.unwrap(),
        vec![completed.record.clone()]
    );
    assert_eq!(
        store.summary(task.root_id).await.unwrap().unsettled_tasks,
        0
    );
    let snapshot = store
        .tasks(task.root_id, AgentTaskFilter::default())
        .await
        .unwrap();
    assert_eq!(
        store.settle_task(task.clone(), completion).await.unwrap(),
        AgentMailAcceptance {
            record: completed.record,
            inserted: false
        }
    );
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap(),
        snapshot
    );
    store.remove_root(task.root_id).await.unwrap();
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap(),
        AgentTaskSnapshot::default()
    );
    assert_eq!(
        store.pending(task.root_id, task.owner_id).await.unwrap(),
        vec![]
    );
}

#[tokio::test]
async fn conflicts_cannot_partially_create_or_settle_ephemeral_tasks() {
    let store = InMemoryAgentMailboxStore::default();
    let (mut task, request) = fixture();
    let mut collision = request.clone();
    collision.payload_json = "{\"different\":true}".into();
    store.accept(collision).await.unwrap();
    let before = store.summary(task.root_id).await.unwrap();
    assert!(
        store
            .accept_task(task.clone(), request.clone())
            .await
            .is_err()
    );
    assert_eq!(store.summary(task.root_id).await.unwrap(), before);
    assert!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks
            .is_empty()
    );

    store.put_task(task.clone()).await.unwrap();
    let before = store
        .tasks(task.root_id, AgentTaskFilter::default())
        .await
        .unwrap();
    task.status = AgentTaskStatus::Failed;
    let mut completion = request;
    completion.sender_id = task.target_id;
    completion.recipient_id = task.owner_id;
    assert!(store.settle_task(task.clone(), completion).await.is_err());
    assert_eq!(
        store
            .tasks(task.root_id, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );
}
