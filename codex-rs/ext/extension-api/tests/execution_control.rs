use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionControlSnapshot;
use codex_extension_api::ExecutionMode;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn holds_descendants_without_cancelling_admitted_work() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let grandchild = ThreadId::new();
    let unrelated = ThreadId::new();
    gate.inherit(child, root);
    gate.inherit(grandchild, child);
    gate.apply(root, 1, ExecutionMode::Running);

    let running = gate.try_enter(child).expect("initial child admission");
    gate.apply(root, 2, ExecutionMode::Paused);
    assert!(gate.try_enter(root).is_none());
    assert!(gate.try_enter(grandchild).is_none());
    assert!(gate.try_enter(unrelated).is_some());
    assert_eq!(
        gate.snapshot(root),
        Some(ExecutionControlSnapshot {
            revision: 2,
            mode: ExecutionMode::Paused,
            reconciling: false,
            in_flight: 1,
        })
    );

    // Completion is still allowed while admission is closed.
    drop(running);
    assert_eq!(gate.snapshot(child).expect("enrolled family").in_flight, 0);
    let mut waiter = std::pin::pin!(gate.enter(grandchild));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    gate.apply(root, 3, ExecutionMode::Running);
    drop(waiter.await);
    assert_eq!(gate.snapshot(root).expect("enrolled family").in_flight, 0);
}

#[tokio::test]
async fn stale_controls_and_new_children_cannot_escape_a_stop() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    gate.apply(root, 5, ExecutionMode::Stopped);
    gate.apply(root, 4, ExecutionMode::Running);
    gate.apply(root, 5, ExecutionMode::Running);
    let child = ThreadId::new();
    gate.inherit(child, root);
    assert_eq!(
        gate.snapshot(child),
        Some(ExecutionControlSnapshot {
            revision: 5,
            mode: ExecutionMode::Stopped,
            reconciling: false,
            in_flight: 0,
        })
    );
    assert!(gate.try_enter(child).is_none());

    // Dropping a waiter must not consume a permit or change the durable mode.
    let mut waiter = Box::pin(gate.enter(child));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(waiter);
    gate.apply(root, 6, ExecutionMode::Running);
    assert!(gate.try_enter(child).is_some());
}

#[tokio::test]
async fn restored_running_state_requires_matching_coordinator_confirmation() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    gate.restore(root, 7, ExecutionMode::Running);
    assert!(gate.try_enter(root).is_none());

    // Neither an old control nor conflicting content at the same revision is
    // evidence that the coordinator has reconciled this host.
    gate.apply(root, 6, ExecutionMode::Running);
    gate.apply(root, 7, ExecutionMode::Stopped);
    assert!(gate.try_enter(root).is_none());
    gate.apply(root, 7, ExecutionMode::Running);
    assert!(gate.try_enter(root).is_some());
}

#[test]
fn global_resume_preserves_local_interruptions_and_user_input_preserves_global_holds() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    gate.inherit(child, root);
    gate.apply(root, 1, ExecutionMode::Running);
    gate.hold_manually(root);
    gate.apply(root, 2, ExecutionMode::Paused);
    gate.resume_for_user(root);
    assert!(gate.is_manually_held(root));
    gate.apply(root, 3, ExecutionMode::Running);
    assert!(gate.try_enter(root).is_none());
    assert!(gate.try_enter(child).is_none());
    gate.resume_for_user(root);
    assert!(gate.try_enter(child).is_some());
}
