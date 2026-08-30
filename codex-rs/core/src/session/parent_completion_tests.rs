use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;

use super::parent_completion_delivery_id;
use super::terminal_status_from_rollout;
use crate::agent::AgentStatus;

#[test]
fn terminal_marker_is_ignored_after_a_later_turn_starts() {
    let rollout_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: None,
            delivery_id: None,
            terminal_turn_id: Some("completed-turn".to_string()),
            terminal_status: Some(AgentStatus::Completed(Some("done".to_string()))),
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: None,
        },
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "follow-up-turn".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        })),
    ];

    assert_eq!(terminal_status_from_rollout(&rollout_items), None);
}

#[test]
fn terminal_marker_is_ignored_after_a_rollback() {
    let rollout_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            mail_fingerprint: None,
            delivery_id: None,
            terminal_turn_id: Some("rolled-back-turn".to_string()),
            terminal_status: Some(AgentStatus::Completed(Some("stale".to_string()))),
            mail_communication: None,
            mail_start_options: None,
            consumed_mail_id: None,
        },
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    assert_eq!(terminal_status_from_rollout(&rollout_items), None);
}

#[test]
fn parent_completion_delivery_id_bounds_untrusted_turn_id() {
    let thread_id = ThreadId::from_u128(42);
    let turn_id = "turn".repeat(50_000);

    let first = parent_completion_delivery_id(thread_id, &turn_id);
    let second = parent_completion_delivery_id(thread_id, &turn_id);

    assert_eq!(first, second);
    assert!(first.as_str().len() < 80);
}
