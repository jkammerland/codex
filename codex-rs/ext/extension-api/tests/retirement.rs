use codex_extension_api::ExecutionControl;
use codex_extension_api::ExecutionMode;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn retirement_drains_existing_operations_but_rejects_new_turns_for_the_family() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    gate.inherit(child, root);
    gate.apply(root, 1, ExecutionMode::Running);
    let started = gate.try_start_turn(child).unwrap();
    gate.begin_retirement(root);

    assert!(gate.try_start_turn(root).is_none());
    assert!(gate.try_start_turn(child).is_none());
    let tool = gate
        .try_enter(child)
        .expect("admitted turn can finish tools");
    assert!(!gate.complete_retirement(root));
    drop(started);
    assert!(!gate.complete_retirement(root));
    drop(tool);
    assert!(gate.complete_retirement(root));
    assert_eq!(gate.root(child), None);
}

#[test]
fn final_detach_cannot_override_a_new_manual_or_global_hold() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    let child = ThreadId::new();
    gate.inherit(child, root);
    gate.apply(root, 1, ExecutionMode::Running);
    gate.begin_retirement(root);
    assert_eq!(gate.retirement_blocker(root), None);

    // A hold arriving after the host's initial check wins at final detachment.
    gate.hold_manually(child);
    assert!(!gate.complete_retirement(root));
    assert_eq!(gate.root(child), Some(root));
    gate.resume_for_user(child);
    gate.apply(root, 2, ExecutionMode::Paused);
    assert!(!gate.complete_retirement(root));
    gate.apply_reconciling(root, 3, ExecutionMode::Running, true);
    assert!(!gate.complete_retirement(root));
    gate.apply(root, 3, ExecutionMode::Running);
    assert!(gate.complete_retirement(root));
}

#[test]
fn explicit_recovery_can_finish_admitted_work_but_never_bypasses_holds() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    gate.apply(root, 1, ExecutionMode::Running);
    gate.begin_retirement(root);
    gate.hold_manually(root);
    assert!(gate.try_resume_turn(root).is_none());
    gate.resume_for_user(root);
    gate.apply(root, 2, ExecutionMode::Stopped);
    assert!(gate.try_resume_turn(root).is_none());
    gate.apply(root, 3, ExecutionMode::Running);
    assert!(gate.try_start_turn(root).is_none());
    let recovery = gate.try_resume_turn(root).unwrap();
    assert_eq!(gate.pending_turn_starts(root), 1);
    assert!(!gate.complete_retirement(root));
    drop(recovery);
    assert_eq!(gate.pending_turn_starts(root), 0);
    assert!(gate.complete_retirement(root));
}

#[test]
fn final_checkpoint_defers_recovery_without_consuming_it_and_preserves_priority_holds() {
    let gate = ExecutionControl::default();
    let root = ThreadId::new();
    gate.apply(root, 1, ExecutionMode::Running);
    gate.begin_retirement(root);
    let checkpoint = gate.try_checkpoint_retirement(root).unwrap();
    assert!(gate.try_resume_turn(root).is_none());
    gate.apply(root, 2, ExecutionMode::Paused);
    assert!(!gate.complete_retirement(root));
    let mut changed = gate.subscribe();
    changed.borrow_and_update();
    drop(checkpoint);
    assert!(changed.has_changed().unwrap());
    assert!(gate.try_resume_turn(root).is_none());
    gate.apply(root, 3, ExecutionMode::Running);
    let recovery = gate.try_resume_turn(root).unwrap();
    assert!(gate.try_checkpoint_retirement(root).is_none());
    drop(recovery);
    assert!(gate.try_checkpoint_retirement(root).is_some());
}
