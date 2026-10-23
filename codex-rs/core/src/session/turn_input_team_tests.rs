//! Team steering must close before the regular task hands off to final recording.

use super::*;
use codex_protocol::turn_input::TeamMessageInput;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(false; "terminal_error")]
#[test_case(true; "cancelled")]
#[tokio::test]
async fn failed_turn_rejects_team_input_before_final_recording(cancelled: bool) {
    let (session, turn, _rx) = make_session_and_context_with_rx().await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
    if cancelled {
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            .cancellation_token
            .cancel();
    } else {
        *turn.terminal_error.lock().await = Some(ErrorEvent {
            message: "terminal test failure".into(),
            codex_error_info: None,
            misalignment: None,
        });
    }
    let result = handle(
        &session,
        TurnInputRequest::new(SubmittedTurnInput::TeamMessage(TeamMessageInput {
            message_id: "late".into(),
            delivery_id: "late-delivery".into(),
            sender: "teammate".into(),
            body: "keep this pending".into(),
            reply_to: None,
            board: None,
        })),
        TurnInputMode::Steer {
            expected_turn_id: turn.sub_id.clone(),
        },
        "late".into(),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn
        }
    );
    assert!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .0
            .is_empty()
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn final_pending_check_rejects_late_team_input_without_consuming_it() {
    let (session, turn, _rx) = make_session_and_context_with_rx().await;
    session
        .spawn_task(
            Arc::clone(&turn),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

    let request = |id: &str| {
        TurnInputRequest::new(SubmittedTurnInput::TeamMessage(TeamMessageInput {
            message_id: id.into(),
            delivery_id: format!("delivery-{id}"),
            sender: "teammate".into(),
            body: format!("work from {id}"),
            reply_to: None,
            board: None,
        }))
    };
    let steer = TurnInputMode::Steer {
        expected_turn_id: turn.sub_id.clone(),
    };
    let before = handle(&session, request("before"), steer.clone(), "before".into())
        .await
        .unwrap();
    assert!(matches!(before, TurnInputSubmission::Steered { .. }));

    // Input already accepted before the final check still gets a sampling opportunity.
    assert!(
        !session
            .input_queue
            .close_team_input_if_idle(&session.active_turn)
            .await
    );
    let (input, _) = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    assert_eq!(input.len(), 1);
    let TurnInput::ResponseItem(item) = &input[0] else {
        panic!("expected attributed team input");
    };
    assert!(
        serde_json::to_string(&item.item)
            .unwrap()
            .contains("work from before")
    );

    // Deterministically stop between the final check and on_task_finished.
    assert!(
        session
            .input_queue
            .close_team_input_if_idle(&session.active_turn)
            .await
    );
    let late = handle(&session, request("late"), steer, "late".into())
        .await
        .unwrap();
    assert_eq!(
        late,
        TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::NoActiveTurn,
        }
    );
    let (input, _) = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    assert!(
        input.is_empty(),
        "the rejected delivery stays with its host"
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}
