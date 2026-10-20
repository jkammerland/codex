//! Converts agent messages into attributed input while preserving their wake mode.

use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;

impl AgentMessage {
    pub(crate) fn into_communication(
        self,
        author: AgentPath,
        recipient: AgentPath,
        mode: MessageDeliveryMode,
    ) -> InterAgentCommunication {
        let trigger_turn = mode == MessageDeliveryMode::TriggerTurn;
        let mut communication = match self {
            Self::Encrypted(message) => InterAgentCommunication::new_encrypted(
                author,
                recipient,
                Vec::new(),
                message,
                trigger_turn,
            ),
            Self::Plaintext(message) => {
                let message_type = match mode {
                    MessageDeliveryMode::Auto | MessageDeliveryMode::QueueOnly => {
                        InterAgentMessageType::Message
                    }
                    MessageDeliveryMode::TriggerTurn => InterAgentMessageType::NewTask,
                };
                let content = InterAgentMessage::new(
                    message_type,
                    recipient.clone(),
                    author.clone(),
                    message,
                )
                .render()
                .into_text()
                .unwrap_or_default();
                InterAgentCommunication::new(author, recipient, Vec::new(), content, trigger_turn)
            }
        };
        communication.mail = Some(codex_protocol::protocol::AgentMailMetadata {
            message_id: codex_protocol::ResponseItemId::new("agentmail").to_string(),
            task_id: trigger_turn.then(|| uuid::Uuid::now_v7().to_string()),
            kind: if trigger_turn {
                codex_protocol::protocol::AgentMailKind::Assignment
            } else {
                codex_protocol::protocol::AgentMailKind::DirectMessage
            },
            wake: if mode == MessageDeliveryMode::QueueOnly {
                codex_protocol::protocol::AgentMailWake::QueueOnly
            } else {
                codex_protocol::protocol::AgentMailWake::Auto
            },
        });
        communication
    }
}
