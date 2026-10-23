use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::turn_input::TeamMessageInput;

use super::ContextualUserFragment;

pub(crate) struct TeamMessage(pub TeamMessageInput);

impl ContextualUserFragment for TeamMessage {
    fn role(&self) -> &'static str {
        "assistant"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("team.peer_message".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<team_message>", "</team_message>")
    }

    fn body(&self) -> ContentItem {
        let mut payload = serde_json::json!({
            "messageId": self.0.message_id,
            "deliveryId": self.0.delivery_id,
            "sender": self.0.sender,
            "replyTo": self.0.reply_to,
            "body": self.0.body,
        });
        if let Some(board) = &self.0.board {
            payload["board"] = serde_json::json!(board);
        }
        ContentItem::InputText {
            text: format!("\n{}\n", payload.to_string().replace('<', "\\u003c")),
        }
    }
}
