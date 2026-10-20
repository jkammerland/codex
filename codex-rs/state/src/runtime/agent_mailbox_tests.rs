use super::*;
use crate::StateRuntime;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

async fn runtime() -> Arc<StateRuntime> {
    let home = unique_temp_dir();
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.abs()),
        "test-provider".to_string(),
    )
    .await
    .unwrap()
}

fn task(root_id: ThreadId, target_id: ThreadId) -> AgentTaskRecord {
    AgentTaskRecord {
        task_id: "task-1".to_string(),
        root_id,
        owner_id: root_id,
        target_id,
        name: Some("review".to_string()),
        status: AgentTaskStatus::Queued,
        turn_id: None,
        result_ref: None,
    }
}

fn request(task: &AgentTaskRecord, message_id: &str) -> AgentMailEnvelope {
    AgentMailEnvelope {
        message_id: message_id.to_string(),
        root_id: task.root_id,
        sender_id: task.owner_id,
        recipient_id: task.target_id,
        kind: "task_request".to_string(),
        payload_json: r#"{"message":"review this"}"#.to_string(),
        wake: true,
        task_id: Some(task.task_id.clone()),
    }
}

#[tokio::test]
async fn concurrent_acceptance_is_idempotent_and_conflicts_do_not_mutate() {
    let runtime = runtime().await;
    let other = StateRuntime::init(runtime.sqlite().clone(), "test-provider".to_string())
        .await
        .unwrap();
    let root = ThreadId::new();
    let target = ThreadId::new();
    let envelope = request(&task(root, target), "message-1");
    let (first, second) = tokio::join!(
        runtime.agent_mailbox().accept(envelope.clone()),
        other.agent_mailbox().accept(envelope.clone()),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first.inserted, second.inserted);
    assert_eq!(first.record, second.record);
    assert_eq!(first.record.envelope, envelope);

    let before = runtime.agent_mailbox().summary(root).await.unwrap();
    let mut conflict = envelope.clone();
    conflict.payload_json = r#"{"message":"different"}"#.to_string();
    assert!(runtime.agent_mailbox().accept(conflict).await.is_err());
    assert_eq!(runtime.agent_mailbox().summary(root).await.unwrap(), before);
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![first.record]
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .pending(ThreadId::new(), target)
            .await
            .unwrap(),
        vec![]
    );
    other.close().await;
    runtime.close().await;
    assert!(runtime.agent_mailbox().accept(envelope).await.is_err());
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn recorded_mail_survives_reopen_and_submission_marks_only_included_ids() {
    let runtime = runtime().await;
    let sqlite = runtime.sqlite().clone();
    let root = ThreadId::new();
    let target = ThreadId::new();
    let task = task(root, target);
    let envelope = request(&task, "first");
    let accepted = runtime
        .agent_mailbox()
        .accept_task(task.clone(), envelope.clone())
        .await
        .unwrap();
    let mut second = request(&task, "second");
    second.task_id = None;
    second.wake = false;
    let second_record = runtime.agent_mailbox().accept(second).await.unwrap().record;
    assert!(
        runtime
            .agent_mailbox()
            .mark_submitted(root, target, vec!["first".into()], "turn-1".into())
            .await
            .is_err()
    );
    assert!(
        runtime
            .agent_mailbox()
            .mark_recorded(
                root,
                target,
                vec!["first".into(), "missing".into()],
                "turn-1".into()
            )
            .await
            .is_err()
    );
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![accepted.record.clone(), second_record.clone()]
    );

    runtime
        .agent_mailbox()
        .mark_recorded(root, target, vec!["first".into()], "turn-1".into())
        .await
        .unwrap();
    runtime.close().await;
    drop(runtime);
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .unwrap();
    let mut recorded = accepted.record;
    recorded.recorded_turn_id = Some("turn-1".into());
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![recorded.clone(), second_record.clone()]
    );
    runtime
        .agent_mailbox()
        .mark_submitted(root, target, vec!["first".into()], "turn-2".into())
        .await
        .unwrap();
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![second_record]
    );
    recorded.submitted_turn_id = Some("turn-2".into());
    assert_eq!(
        runtime
            .agent_mailbox()
            .accept(envelope.clone())
            .await
            .unwrap(),
        AgentMailAcceptance {
            record: recorded,
            inserted: false
        }
    );

    let mut conflicting_task = task.clone();
    conflicting_task.name = Some("different assignment".into());
    assert!(
        runtime
            .agent_mailbox()
            .accept_task(conflicting_task, envelope.clone())
            .await
            .is_err()
    );

    // Retrying the original acceptance must preserve the actual execution turn.
    runtime
        .agent_mailbox()
        .accept_task(task.clone(), envelope)
        .await
        .unwrap();
    let mut running = task;
    running.status = AgentTaskStatus::Running;
    running.turn_id = Some("turn-2".into());
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![running]
    );
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn attempted_input_binds_only_selected_tasks_without_creating_submission_receipts() {
    let runtime = runtime().await;
    let root = ThreadId::new();
    let target = ThreadId::new();
    let first = task(root, target);
    let accepted = runtime
        .agent_mailbox()
        .accept_task(first.clone(), request(&first, "selected"))
        .await
        .unwrap();
    let before = runtime
        .agent_mailbox()
        .tasks(root, AgentTaskFilter::default())
        .await
        .unwrap();
    assert!(
        runtime
            .agent_mailbox()
            .mark_attempted(
                root,
                target,
                vec!["selected".into(), "missing".into()],
                "attempt".into()
            )
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );
    runtime
        .agent_mailbox()
        .mark_attempted(root, target, vec!["selected".into()], "attempt".into())
        .await
        .unwrap();
    let mut later = first.clone();
    later.task_id = "task-2".into();
    let late_mail = runtime
        .agent_mailbox()
        .accept_task(later.clone(), request(&later, "late"))
        .await
        .unwrap();
    let mut attempted = first;
    attempted.turn_id = Some("attempt".into());
    let snapshot = runtime
        .agent_mailbox()
        .tasks(root, AgentTaskFilter::default())
        .await
        .unwrap();
    assert_eq!(snapshot.tasks, vec![attempted, later]);
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![accepted.record, late_mail.record]
    );
    runtime
        .agent_mailbox()
        .mark_attempted(root, target, vec!["selected".into()], "attempt".into())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        snapshot
    );
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn held_tasks_rebind_to_included_turn_without_resetting_receipts_or_acceptance() {
    let runtime = runtime().await;
    let root = ThreadId::new();
    let target = ThreadId::new();
    let initial = task(root, target);
    let envelope = request(&initial, "request");
    runtime
        .agent_mailbox()
        .accept_task(initial.clone(), envelope.clone())
        .await
        .unwrap();
    runtime
        .agent_mailbox()
        .mark_recorded(root, target, vec!["request".into()], "turn-a".into())
        .await
        .unwrap();
    runtime
        .agent_mailbox()
        .mark_submitted(root, target, vec!["request".into()], "turn-a".into())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .agent_mailbox()
            .accepted_ids(
                root,
                target,
                vec!["unknown".into(), "request".into(), "request".into()]
            )
            .await
            .unwrap(),
        vec!["request".to_string()]
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .accepted_ids(root, root, vec!["request".into()])
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    let before = runtime
        .agent_mailbox()
        .tasks(root, AgentTaskFilter::default())
        .await
        .unwrap();
    assert!(
        runtime
            .agent_mailbox()
            .accept_task(initial, request(&before.tasks[0], "second-request"))
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![]
    );

    let mut resumed = before.tasks[0].clone();
    resumed.status = AgentTaskStatus::Held;
    runtime
        .agent_mailbox()
        .put_task(resumed.clone())
        .await
        .unwrap();
    runtime
        .agent_mailbox()
        .mark_submitted(root, target, vec!["request".into()], "turn-b".into())
        .await
        .unwrap();
    resumed.status = AgentTaskStatus::Running;
    resumed.turn_id = Some("turn-b".into());
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![resumed]
    );
    let first_receipt = runtime
        .agent_mailbox()
        .accept(envelope)
        .await
        .unwrap()
        .record;
    assert_eq!(
        (
            first_receipt.recorded_turn_id,
            first_receipt.submitted_turn_id
        ),
        (Some("turn-a".into()), Some("turn-a".into()))
    );
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn conflicting_task_creation_cannot_leave_an_orphan_request() {
    let runtime = runtime().await;
    let root = ThreadId::new();
    let target = ThreadId::new();
    let task = task(root, target);
    let accepted = runtime
        .agent_mailbox()
        .accept_task(task.clone(), request(&task, "first"))
        .await
        .unwrap();
    let before = runtime
        .agent_mailbox()
        .tasks(root, AgentTaskFilter::default())
        .await
        .unwrap();
    let mut conflict = task.clone();
    conflict.name = Some("different task".into());
    assert!(
        runtime
            .agent_mailbox()
            .accept_task(conflict.clone(), request(&conflict, "orphan"))
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![accepted.record]
    );
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn task_settlement_rolls_back_with_conflicting_mail_and_survives_reopen() {
    let runtime = runtime().await;
    let sqlite = runtime.sqlite().clone();
    let root = ThreadId::new();
    let target = ThreadId::new();
    let queued = task(root, target);
    runtime
        .agent_mailbox()
        .accept_task(queued.clone(), request(&queued, "request"))
        .await
        .unwrap();
    let mut completion = request(&queued, "completion");
    completion.sender_id = target;
    completion.recipient_id = root;
    completion.kind = "task_result".into();
    completion.payload_json = r#"{"result":"done"}"#.into();
    let mut collision = completion.clone();
    collision.payload_json = r#"{"other":true}"#.into();
    runtime.agent_mailbox().accept(collision).await.unwrap();
    let before = runtime
        .agent_mailbox()
        .tasks(root, AgentTaskFilter::default())
        .await
        .unwrap();
    let mut terminal = queued.clone();
    terminal.status = AgentTaskStatus::Completed;
    terminal.turn_id = Some("turn-1".into());
    terminal.result_ref = Some("result-1".into());
    assert!(
        runtime
            .agent_mailbox()
            .settle_task(terminal.clone(), completion.clone())
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        before
    );

    completion.message_id = "completion-success".into();
    let accepted = runtime
        .agent_mailbox()
        .settle_task(terminal.clone(), completion.clone())
        .await
        .unwrap();
    assert!(accepted.inserted);
    let summary = runtime.agent_mailbox().summary(root).await.unwrap();
    assert_eq!(summary.unsettled_tasks, 0);
    runtime.close().await;
    drop(runtime);
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .unwrap();
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![terminal.clone()]
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .settle_task(terminal, completion)
            .await
            .unwrap(),
        AgentMailAcceptance {
            record: accepted.record,
            inserted: false
        }
    );
    assert_eq!(
        runtime.agent_mailbox().summary(root).await.unwrap(),
        summary
    );
    assert!(runtime.agent_mailbox().put_task(queued).await.is_err());
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}

#[tokio::test]
async fn snapshots_filter_tasks_and_root_lifecycle_removes_only_its_records() {
    let runtime = runtime().await;
    let root = ThreadId::new();
    let other_root = ThreadId::new();
    let target = ThreadId::new();
    let first = task(root, target);
    let mut held = first.clone();
    held.task_id = "held".into();
    held.status = AgentTaskStatus::Held;
    let mut unavailable = first.clone();
    unavailable.task_id = "unavailable".into();
    unavailable.status = AgentTaskStatus::Unavailable;
    let mut other = task(other_root, target);
    other.task_id = "other-root".into();
    for task in [&first, &held, &unavailable, &other] {
        runtime
            .agent_mailbox()
            .accept_task(task.clone(), request(task, &task.task_id))
            .await
            .unwrap();
    }
    let filter = AgentTaskFilter {
        task_ids: vec![held.task_id.clone()],
        owner_id: Some(root),
        target_id: Some(target),
        statuses: vec![AgentTaskStatus::Held],
    };
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, filter)
            .await
            .unwrap()
            .tasks,
        vec![held]
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .summary(root)
            .await
            .unwrap()
            .unsettled_tasks,
        3
    );
    let other_summary = runtime.agent_mailbox().summary(other_root).await.unwrap();
    runtime.delete_thread(root).await.unwrap();
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(root, AgentTaskFilter::default())
            .await
            .unwrap(),
        AgentTaskSnapshot::default()
    );
    assert_eq!(
        runtime.agent_mailbox().pending(root, target).await.unwrap(),
        vec![]
    );
    assert_eq!(
        runtime.agent_mailbox().summary(other_root).await.unwrap(),
        other_summary
    );
    assert_eq!(
        runtime
            .agent_mailbox()
            .tasks(other_root, AgentTaskFilter::default())
            .await
            .unwrap()
            .tasks,
        vec![other]
    );
    runtime.close().await;
    tokio::fs::remove_dir_all(runtime.sqlite().home())
        .await
        .unwrap();
}
