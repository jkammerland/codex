//! Board delivery enrichment leaves the persisted delivery envelope unchanged.

use codex_app_server_protocol::*;
use codex_protocol::turn_input::*;

pub(crate) fn board_context(
    entry: &TeamBoardEntry,
    recipient: &TeamAddress,
) -> TeamMessageBoardContext {
    let recipient = TeamBoardIdentity::Agent {
        address: recipient.clone(),
    };
    TeamMessageBoardContext {
        conversation_id: entry.conversation_id.clone(),
        append_seq: entry.append_seq,
        author: TeamMessageAuthor {
            identity: context_identity(&entry.author.identity),
            name: entry.author.name.clone(),
            machine_name: entry.author.machine_name.clone(),
            legacy: entry.author.legacy,
        },
        mentions: entry
            .mentions
            .iter()
            .map(|mention| TeamMessageMention {
                identity: context_identity(&mention.identity),
                display: mention.display.clone(),
            })
            .collect(),
        notification_reasons: entry
            .notifications
            .iter()
            .find(|notification| notification.identity == recipient)
            .map(|notification| {
                notification
                    .reasons
                    .iter()
                    .map(|reason| match reason {
                        TeamBoardNotificationReason::Mention => {
                            TeamMessageNotificationReason::Mention
                        }
                        TeamBoardNotificationReason::ConversationFollow => {
                            TeamMessageNotificationReason::ConversationFollow
                        }
                        TeamBoardNotificationReason::TopicSubscription => {
                            TeamMessageNotificationReason::TopicSubscription
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn context_identity(identity: &TeamBoardIdentity) -> TeamMessageIdentity {
    match identity {
        TeamBoardIdentity::User => TeamMessageIdentity::User,
        TeamBoardIdentity::Agent { address } => TeamMessageIdentity::Agent {
            machine_id: address.machine_id.clone(),
            server_id: address.server_id.clone(),
            thread_id: address.thread_id.clone(),
        },
    }
}
