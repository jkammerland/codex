use codex_history::RolloutItem;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;

use super::terminal_status_from_rollout;
use crate::agent::AgentStatus;

#[test]
fn terminal_marker_is_ignored_after_a_later_turn_starts() {
    let rollout_items = vec![
        RolloutItem::TerminalAgentStatus {
            turn_id: "completed-turn".to_string(),
            status: AgentStatus::Completed(Some("done".to_string())),
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
