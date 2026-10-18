//! Existing Board controls expose explicit passive and wake subscription modes.

use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::TeamBoardAuthor;
use codex_app_server_protocol::TeamBoardIdentity;
use codex_app_server_protocol::TeamBoardParams;
use codex_app_server_protocol::TeamBoardSnapshot;
use codex_app_server_protocol::TeamBoardSubscriptionMode;
use codex_app_server_protocol::TeamBoardSubscriptionTarget;

impl App {
    pub(super) fn open_agents_board_subscriptions(&mut self, target: TeamBoardSubscriptionTarget) {
        let state = self
            .agents_hosts
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let items = subscription_participants(&state.snapshot, &target)
            .into_iter()
            .map(|participant| {
                let identity = participant.author.identity.clone();
                let name = if identity == TeamBoardIdentity::User {
                    "You".into()
                } else {
                    participant.author.name.clone()
                };
                let action_name = name.clone();
                let status = state.subscription_status(&target, &identity);
                let action_target = target.clone();
                SelectionItem {
                    name: format!("{name} · {status}"),
                    description: Some(format!(
                        "{} · {}",
                        participant
                            .author
                            .machine_name
                            .as_deref()
                            .unwrap_or("Human"),
                        if !participant.enrolled {
                            "no longer enrolled · Off remains available"
                        } else if participant.connected {
                            "connected"
                        } else {
                            "connection unavailable"
                        }
                    )),
                    actions: vec![Box::new(move |sender| {
                        sender
                            .unscoped()
                            .send(AppEvent::AgentsBoardSubscriptionMode {
                                target: action_target.clone(),
                                identity: identity.clone(),
                                name: action_name.clone(),
                            })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        drop(state);
        let title = match &target {
            TeamBoardSubscriptionTarget::Conversation { .. } => "Conversation subscriptions".into(),
            TeamBoardSubscriptionTarget::Topic { topic } => format!("Subscriptions · #{topic}"),
        };
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some(title),
            subtitle: Some(
                "Watch is passive · Wake delivers updates · select an identity to change".into(),
            ),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn open_agents_board_subscription_mode(
        &mut self,
        target: TeamBoardSubscriptionTarget,
        identity: TeamBoardIdentity,
        name: String,
    ) {
        let state = self
            .agents_hosts
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = state.subscription_status(&target, &identity);
        let pending = state
            .pending_subscriptions
            .values()
            .any(|pending| pending.target == target && pending.identity == identity);
        let enrolled = identity == TeamBoardIdentity::User
            || state
                .snapshot
                .members
                .iter()
                .any(|member| member.author.identity == identity);
        drop(state);
        let mut choices = vec![(
            TeamBoardSubscriptionMode::Watch,
            "Watch",
            "Following and unread only; never inserts turn input",
        )];
        if identity != TeamBoardIdentity::User {
            choices.push((
                TeamBoardSubscriptionMode::Wake,
                "Wake",
                match target {
                    TeamBoardSubscriptionTarget::Conversation { .. } => {
                        "Deliver replies from others; may start the agent"
                    }
                    TeamBoardSubscriptionTarget::Topic { .. } => {
                        "Deliver new root posts; replies require a conversation follow"
                    }
                },
            ));
        }
        choices.push((
            TeamBoardSubscriptionMode::Off,
            "Off",
            "Remove this subscription; accepted deliveries remain queued",
        ));
        let items = choices
            .into_iter()
            .map(|(mode, label, description)| {
                let target = target.clone();
                let identity = identity.clone();
                SelectionItem {
                    name: label.into(),
                    description: Some(description.into()),
                    disabled_reason: if pending {
                        Some(
                            "A change is pending; return to Board and use Ctrl+R if unconfirmed"
                                .into(),
                        )
                    } else if !enrolled && mode != TeamBoardSubscriptionMode::Off {
                        Some("Agent is no longer enrolled; only Off is available".into())
                    } else {
                        None
                    },
                    actions: vec![Box::new(move |sender| {
                        sender.unscoped().send(AppEvent::AgentsTeamRequest(
                            codex_team::TeamRequest::Board {
                                request: TeamBoardParams::SubscriptionSet {
                                    target: target.clone(),
                                    identity: Some(identity.clone()),
                                    mode,
                                },
                            },
                        ))
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some(format!("Subscription · {name} · {current}")),
            subtitle: Some(
                "Explicit mentions deliver independently of Watch · changes affect future messages"
                    .into(),
            ),
            items,
            ..SelectionViewParams::picker()
        });
    }
}

struct SubscriptionParticipant {
    author: TeamBoardAuthor,
    enrolled: bool,
    connected: bool,
}

fn subscription_participants(
    snapshot: &TeamBoardSnapshot,
    target: &TeamBoardSubscriptionTarget,
) -> Vec<SubscriptionParticipant> {
    // The operator remains selectable even before membership has synchronized.
    let mut participants = vec![SubscriptionParticipant {
        author: TeamBoardAuthor {
            identity: TeamBoardIdentity::User,
            name: "User".into(),
            machine_name: None,
            legacy: false,
        },
        enrolled: true,
        connected: true,
    }];
    let mut seen = std::collections::HashSet::from([TeamBoardIdentity::User]);
    for member in &snapshot.members {
        if seen.insert(member.author.identity.clone()) {
            participants.push(SubscriptionParticipant {
                author: member.author.clone(),
                enrolled: true,
                connected: member.available,
            });
        }
    }

    let identities = match target {
        TeamBoardSubscriptionTarget::Conversation { conversation_id } => snapshot
            .followers
            .iter()
            .filter(|follower| follower.conversation_id == *conversation_id)
            .map(|follower| &follower.identity)
            .collect::<Vec<_>>(),
        TeamBoardSubscriptionTarget::Topic { topic } => snapshot
            .topic_subscriptions
            .iter()
            .filter(|subscription| subscription.topic == *topic)
            .map(|subscription| &subscription.identity)
            .collect::<Vec<_>>(),
    };
    for identity in identities {
        if !seen.insert(identity.clone()) {
            continue;
        }
        // Conversation followers retain their last confirmed name when leaving.
        // Topics can reuse a known author; otherwise display the exact address.
        let retained = snapshot
            .followers
            .iter()
            .find(|follower| &follower.identity == identity)
            .map(|follower| follower.author.clone())
            .or_else(|| {
                snapshot
                    .entries
                    .iter()
                    .filter(|entry| &entry.author.identity == identity)
                    .max_by_key(|entry| entry.append_seq)
                    .map(|entry| entry.author.clone())
            });
        let author = retained.unwrap_or_else(|| match identity {
            TeamBoardIdentity::Agent { address } => TeamBoardAuthor {
                identity: identity.clone(),
                name: format!(
                    "{}/{}/{}",
                    address.machine_id, address.server_id, address.thread_id
                ),
                machine_name: Some(address.machine_id.clone()),
                legacy: true,
            },
            TeamBoardIdentity::User => unreachable!("operator was added first"),
        });
        participants.push(SubscriptionParticipant {
            author,
            enrolled: false,
            connected: false,
        });
    }
    participants
}

#[cfg(test)]
#[path = "agents_board_subscriptions_tests.rs"]
mod tests;
