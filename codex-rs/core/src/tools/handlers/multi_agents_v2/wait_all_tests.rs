use super::*;
use pretty_assertions::assert_eq;

fn task(owner_id: ThreadId, status: AgentTaskStatus) -> AgentTaskRecord {
    AgentTaskRecord {
        task_id: "accepted-1".to_string(),
        root_id: owner_id,
        owner_id,
        target_id: ThreadId::new(),
        name: Some("/root/worker".to_string()),
        status,
        turn_id: None,
        result_ref: None,
    }
}

#[test]
fn wait_all_normalizes_duplicates_and_rejects_invalid_arguments() {
    let (ids, deadline) = WaitAllArgs {
        task_ids: vec!["a".to_string(), "b".to_string(), "a".to_string()],
        timeout_ms: None,
    }
    .validate()
    .unwrap();
    assert_eq!(ids, ["a", "b"]);
    assert_eq!(deadline, None);

    for task_ids in [vec![], vec![String::new()], vec!["  ".to_string()]] {
        assert!(
            WaitAllArgs {
                task_ids,
                timeout_ms: None
            }
            .validate()
            .is_err()
        );
    }
    for timeout_ms in [0, -1] {
        assert!(
            WaitAllArgs {
                task_ids: vec!["a".to_string()],
                timeout_ms: Some(timeout_ms),
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn wait_all_rejects_unknown_foreign_self_and_ancestor_assignments() {
    let owner = ThreadId::new();
    let ids = vec!["accepted-1".to_string()];
    let mut task = task(owner, AgentTaskStatus::Running);
    assert!(validate_tasks(&ids, &[], owner, Some("/root")).is_err());
    assert!(validate_tasks(&ids, &[task.clone()], ThreadId::new(), Some("/root")).is_err());

    task.target_id = owner;
    assert!(validate_tasks(&ids, &[task.clone()], owner, Some("/root")).is_err());
    task.target_id = ThreadId::new();
    assert!(validate_tasks(&ids, &[task.clone()], owner, Some("/root/worker/child")).is_err());
    // A matching textual prefix is not an ancestor path segment.
    assert!(validate_tasks(&ids, &[task.clone()], owner, Some("/root/worker_peer")).is_ok());
    assert!(validate_tasks(&ids, &[task], owner, Some("/root")).is_ok());
}

#[test]
fn wait_all_requires_every_fixed_assignment_to_be_terminal() {
    let owner = ThreadId::new();
    let completed = task(owner, AgentTaskStatus::Completed);
    let mut running = task(owner, AgentTaskStatus::Running);
    running.task_id = "accepted-2".to_string();
    assert_eq!(state_outcome(&[completed.clone(), running.clone()]), None);
    running.status = AgentTaskStatus::Failed;
    assert_eq!(
        state_outcome(&[completed, running]),
        Some(WaitAllReason::AllDone)
    );

    assert_eq!(
        state_outcome(&[task(owner, AgentTaskStatus::Held)]),
        Some(WaitAllReason::Held)
    );
    assert_eq!(
        state_outcome(&[task(owner, AgentTaskStatus::Unavailable)]),
        Some(WaitAllReason::Unavailable)
    );
    assert_eq!(
        state_outcome(&[task(owner, AgentTaskStatus::CapacityUnavailable)]),
        Some(WaitAllReason::CapacityUnavailable)
    );
    assert_eq!(state_outcome(&[]), None);
}

#[tokio::test(start_paused = true)]
async fn wait_all_has_no_default_deadline_and_observation_timeout_is_exact() {
    let indefinite = wait_for_deadline(None);
    tokio::pin!(indefinite);
    assert!(futures::poll!(&mut indefinite).is_pending());
    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
    assert!(futures::poll!(&mut indefinite).is_pending());

    let deadline = Instant::now() + Duration::from_millis(17);
    let explicit = wait_for_deadline(Some(deadline));
    tokio::pin!(explicit);
    assert!(futures::poll!(&mut explicit).is_pending());
    tokio::time::advance(Duration::from_millis(16)).await;
    assert!(futures::poll!(&mut explicit).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(futures::poll!(&mut explicit).is_ready());
}

#[tokio::test(start_paused = true)]
async fn wait_all_deadline_covers_a_blocked_task_store_read() {
    let (_release, read) = tokio::sync::oneshot::channel::<AgentTaskSnapshot>();
    let deadline = Instant::now() + Duration::from_millis(50);
    let observed = observe_tasks(
        async { Ok(read.await.expect("store release")) },
        Some(deadline),
    );
    tokio::pin!(observed);
    assert!(futures::poll!(&mut observed).is_pending());
    tokio::time::advance(Duration::from_millis(50)).await;
    assert_eq!(observed.await, Err(WaitAllReason::Timeout));

    // A completed read cannot turn an already expired observation into all_done.
    let ready = async { Ok(AgentTaskSnapshot::default()) };
    assert_eq!(
        observe_tasks(ready, Some(deadline)).await,
        Err(WaitAllReason::Timeout)
    );
}
