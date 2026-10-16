use codex_app_server_protocol::*;
use codex_team::Agent;
use codex_team::ServerStatus;
use codex_team::TeamSnapshot;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::Panel;

#[test]
fn child_attachment_preserves_relay_paths_and_explicit_default_ports() -> anyhow::Result<()> {
    let address = TeamAddress {
        machine_id: "worker".into(),
        server_id: "primary".into(),
        thread_id: "existing-thread".into(),
    };
    for (base, argument, normalized) in [
        (
            "http://127.0.0.1:4500",
            "ws://127.0.0.1:4500/relay/worker/primary",
            "ws://127.0.0.1:4500/relay/worker/primary",
        ),
        (
            "https://team.example/coordinator",
            "wss://team.example:443/coordinator/relay/worker/primary",
            "wss://team.example/coordinator/relay/worker/primary",
        ),
        (
            "http://[::1]",
            "ws://[::1]:80/relay/worker/primary",
            "ws://[::1]/relay/worker/primary",
        ),
    ] {
        let client = codex_team::TeamClient::new(base, "test-operator-token".into())?;
        let command = client.attach_command(&address)?;
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "--remote",
                argument,
                "--remote-auth-token-env",
                "CODEX_TEAM_RELAY_TOKEN",
                "resume",
                "existing-thread",
            ]
        );
        let endpoint =
            crate::resolve_remote_addr(argument).map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(
            endpoint,
            crate::RemoteAppServerEndpoint::WebSocket {
                websocket_url: normalized.into(),
                auth_token: None,
            }
        );
        assert!(crate::remote_addr_supports_auth_token(&endpoint));
    }
    Ok(())
}

fn panel() -> Panel {
    let agents = [
        ("linux", "builder", "working", false, Some("previous-model")),
        ("windows", "reviewer", "idle", true, None),
    ]
    .into_iter()
    .map(|(machine, alias, status, held, current)| Agent {
        enrollment: TeamEnrollment {
            address: TeamAddress {
                machine_id: machine.into(),
                server_id: "primary".into(),
                thread_id: format!("{machine}-thread"),
            },
            alias: alias.into(),
            manually_held: held,
            continuation_turn_id: None,
        },
        runtime: None,
        status: status.into(),
        current_model: current.map(str::to_owned),
        current_effort: current.map(|_| "medium".into()),
        next_model: Some("chosen-model".into()),
        next_effort: Some("high".into()),
        project: Some("compiler".into()),
    })
    .collect();
    let servers = [
        ("linux", "Linux", true, Some(4)),
        ("windows", "Windows", false, Some(3)),
    ]
    .into_iter()
    .map(|(id, label, connected, revision)| ServerStatus {
        machine_id: id.into(),
        machine_label: label.into(),
        server_id: "primary".into(),
        server_label: "Primary account".into(),
        installation_id: Some(format!("{id}-installation")),
        connected,
        applied_revision: revision,
        error: (!connected).then(|| "Connection lost; Stop remains unconfirmed".into()),
    })
    .collect();
    Panel {
        snapshot: Some(TeamSnapshot {
            rejected_outbox: Vec::new(),
            control: TeamControlState {
                coordinator_id: "coordinator".into(),
                revision: 4,
                mode: TeamExecutionMode::Paused,
            },
            servers,
            agents,
            messages: Vec::new(),
        }),
        connected: true,
        ..Default::default()
    }
}

fn render(panel: &Panel, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    panel.render(area, &mut buffer);
    buffer
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn machine_groups_show_acknowledgements_manual_holds_and_current_vs_next_models() {
    insta::assert_snapshot!(render(&panel(), 110, 26));
}

#[test]
fn narrow_model_picker_keeps_ultra_an_explicit_choice() {
    let mut panel = panel();
    panel.model_target = panel.selected_address();
    panel.models = vec![
        ("chosen-model".into(), "medium".into()),
        ("chosen-model".into(), "ultra".into()),
    ];
    panel.model_selected = 1;
    insta::assert_snapshot!(render(&panel, 64, 16));
}

#[test]
fn board_shows_delivery_reconciliation_and_rejected_outgoing_mail() {
    let mut panel = panel();
    panel.board = true;
    let snapshot = panel.snapshot.as_mut().unwrap();
    let address = snapshot.agents[0].enrollment.address.clone();
    snapshot.messages.push(codex_team::MessageRecord {
        message: TeamMessage {
            id: "request-one".into(),
            sender: address.clone(),
            recipient: None,
            kind: TeamMessageKind::Board,
            body: "Review the compiler change and reply here.".into(),
            topic: Some("reviews".into()),
            reply_to: None,
            created_at: 1,
        },
        delivery: TeamDeliveryMode::NextTurn,
        published: true,
        targets: Some(vec![address]),
        receipts: std::collections::BTreeMap::from([(
            "delivery-one".into(),
            TeamReceipt {
                delivery_id: "delivery-one".into(),
                thread_id: "reviewer".into(),
                state: TeamReceiptState::NeedsReconciliation,
                turn_id: Some("original-turn".into()),
                detail: Some(
                    "Input is durable; inspect the original turn before continuing.".into(),
                ),
            },
        )]),
        error: None,
        external_url: None,
    });
    snapshot.rejected_outbox.push(codex_team::RejectedOutbox {
        machine_id: "windows".into(),
        server_id: "primary".into(),
        entry: TeamOutboxEntry {
            sequence: 12,
            id: "invalid".into(),
            kind: "message".into(),
            payload: serde_json::json!({}),
        },
        reason: "recipient is not enrolled".into(),
    });
    insta::assert_snapshot!(render(&panel, 100, 24));
}

#[test]
fn model_hold_and_unconfirmed_external_work_stay_visible() {
    let mut panel = panel();
    let agent = &mut panel.snapshot.as_mut().unwrap().agents[0];
    agent.runtime = Some(TeamAgentRuntime {
        thread_id: agent.enrollment.address.thread_id.clone(),
        root_thread_id: agent.enrollment.address.thread_id.clone(),
        parent_thread_id: None,
        alias: agent.enrollment.alias.clone(),
        loaded: true,
        archived: false,
        turn_id: Some("original".into()),
        current_model: agent.current_model.clone(),
        current_effort: agent.current_effort.clone(),
        next_model: agent.next_model.clone(),
        next_effort: agent.next_effort.clone(),
        model_error: Some("Model unavailable; select an available next-turn model.".into()),
        in_flight: 1,
        reconciling: false,
        manually_held: false,
        continuation_turn_id: None,
        jobs: vec![TeamTrackedJob {
            id: "owned-job".into(),
            thread_id: agent.enrollment.address.thread_id.clone(),
            server: "jobs".into(),
            tool: "launch".into(),
            handle: Some("owned-handle".into()),
            status: TeamJobStatus::Unconfirmed,
            detail: Some("Cancellation has not been confirmed.".into()),
        }],
    });
    insta::assert_snapshot!(render(&panel, 110, 28));
}
