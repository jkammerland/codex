use super::TurnState;
use pretty_assertions::assert_eq;
use rmcp::model::RequestId;
use tokio::sync::oneshot;

#[test]
fn pending_interactions_follow_live_waiters_and_clear_independently() {
    let mut state = TurnState::default();
    let (approval, approval_response) = oneshot::channel();
    let (input, input_response) = oneshot::channel();
    state.insert_pending_approval("approval".into(), approval);
    state.insert_pending_user_input("input".into(), input);

    assert_eq!(
        (state.has_pending_approval(), state.has_pending_user_input()),
        (true, true)
    );
    state.remove_pending_approval("approval");
    assert_eq!(
        (state.has_pending_approval(), state.has_pending_user_input()),
        (false, true)
    );

    // Cancellation can close a receiver before its map entry is removed.
    drop(input_response);
    assert_eq!(
        (state.has_pending_approval(), state.has_pending_user_input()),
        (false, false)
    );
    drop(approval_response);
}

#[test]
fn pending_elicitation_is_an_approval_wait_until_cleared_or_cancelled() {
    let mut state = TurnState::default();
    let (elicitation, response) = oneshot::channel();
    state.insert_pending_elicitation(
        "server".into(),
        RequestId::String("request".into()),
        elicitation,
    );
    assert!(state.has_pending_approval());
    drop(response);
    assert!(!state.has_pending_approval());

    let (elicitation, _response) = oneshot::channel();
    state.insert_pending_elicitation(
        "server".into(),
        RequestId::String("next-request".into()),
        elicitation,
    );
    assert!(state.has_pending_approval());
    state.clear_pending_waiters();
    assert!(!state.has_pending_approval());
}
