use super::*;

#[test]
fn message_evidence_uses_named_recipients_and_does_not_claim_fulfillment() {
    let address = TeamAddress {
        machine_id: "worker".into(),
        server_id: "main".into(),
        thread_id: "agent".into(),
    };
    let status = TeamMessageStatus {
        message_id: "message".into(),
        sender: address.clone(),
        sender_name: "Author".into(),
        created_at: Some(1),
        intake: TeamMessageIntake::AcceptedAtCoordinator,
        recipients: vec![TeamRecipientMessageStatus {
            address,
            name: "Reviewer".into(),
            progress: TeamMessageProgress::TurnEnded,
            receipt: Some(TeamReceipt {
                delivery_id: "delivery".into(),
                thread_id: "agent".into(),
                state: TeamReceiptState::Completed,
                turn_id: Some("review-turn".into()),
                detail: None,
            }),
            freshness: TeamObservationFreshness::Unconfirmed,
        }],
        related_replies: Vec::new(),
        detail: None,
        freshness: TeamObservationFreshness::Current,
    };
    for width in [48, 100] {
        let header = MessageHeader(message_status_lines(&status));
        let area = Rect::new(0, 0, width, header.desired_height(width));
        let mut buffer = Buffer::empty(area);
        header.render(area, &mut buffer);
        let text = buffer
            .content
            .chunks(usize::from(width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(format!("message_processing_evidence_{width}"), text);
    }
}

#[tokio::test]
async fn action_explanations_show_queueing_separately_from_execution() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let thread_id = ThreadId::from_u128(42);
    let target = ThreadTarget {
        host: Default::default(),
        thread_id,
    };
    let address = target.team_address();
    app.agents_hosts.snapshot = Some(codex_team::TeamSnapshot {
        control: TeamControlState {
            coordinator_id: "team".into(),
            revision: 1,
            mode: TeamExecutionMode::Stopped,
        },
        agents: vec![codex_team::Agent {
            enrollment: TeamEnrollment {
                address,
                alias: "Reviewer".into(),
                manually_held: false,
                continuation_turn_id: None,
            },
            runtime: None,
            discovery: Default::default(),
            status: "unloaded".into(),
            current_model: None,
            current_effort: None,
            next_model: None,
            next_effort: None,
            project: None,
            requested_selection: None,
        }],
        servers: Vec::new(),
        messages: Vec::new(),
        board: Default::default(),
        operations: Vec::new(),
        rejected_outbox: Vec::new(),
    });
    app.open_agents_team_explanations(target);
    insta::assert_snapshot!(
        "team_action_explanations",
        crate::chatwidget::tests::helpers::render_bottom_popup(&app.chat_widget, 100)
    );
}
