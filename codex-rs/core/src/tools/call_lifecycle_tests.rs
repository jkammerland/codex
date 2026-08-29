use super::*;
use pretty_assertions::assert_eq;

#[test]
fn cancellation_before_admission_prevents_handler_start() {
    let lifecycle = ToolCallLifecycle::new();

    assert_eq!(
        lifecycle.cancellation_disposition(/*handler_is_cooperative*/ true),
        CancellationDisposition::AbortDispatch
    );
    assert!(!lifecycle.try_start_handler());
    assert!(!lifecycle.try_claim_terminal());
}

#[test]
fn cooperative_running_handler_retains_terminal_ownership() {
    let lifecycle = ToolCallLifecycle::new();
    assert!(lifecycle.try_start_handler());

    assert_eq!(
        lifecycle.cancellation_disposition(/*handler_is_cooperative*/ true),
        CancellationDisposition::AwaitDispatch
    );
    assert!(lifecycle.cooperative_cancellation_won());
    lifecycle.handler_completed();
    assert!(lifecycle.cooperative_cancellation_won());
    assert!(lifecycle.try_claim_terminal());
}

#[test]
fn noncooperative_running_handler_is_claimed_for_abort() {
    let lifecycle = ToolCallLifecycle::new();
    assert!(lifecycle.try_start_handler());

    assert_eq!(
        lifecycle.cancellation_disposition(/*handler_is_cooperative*/ false),
        CancellationDisposition::AbortDispatch
    );
    lifecycle.handler_completed();
    assert!(!lifecycle.try_claim_terminal());
}

#[test]
fn completed_handler_cannot_be_relabelled_as_aborted() {
    let lifecycle = ToolCallLifecycle::new();
    assert!(lifecycle.try_start_handler());
    lifecycle.handler_completed();

    assert_eq!(
        lifecycle.cancellation_disposition(/*handler_is_cooperative*/ false),
        CancellationDisposition::AwaitDispatch
    );
    assert!(!lifecycle.cooperative_cancellation_won());
    assert!(lifecycle.try_claim_terminal());
}
