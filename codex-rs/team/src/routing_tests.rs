use std::collections::BTreeMap;
use std::collections::HashMap;

use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::ordered_delivery_records;
use crate::model::MessageRecord;

fn record(id: &str, kind: TeamMessageKind) -> (String, MessageRecord) {
    (
        id.into(),
        MessageRecord {
            message: TeamMessage {
                id: id.into(),
                sender: TeamAddress {
                    machine_id: "worker".into(),
                    server_id: "primary".into(),
                    thread_id: "sender".into(),
                },
                recipient: None,
                kind,
                body: id.into(),
                reply_to: None,
                topic: Some("topic".into()),
                created_at: 100,
            },
            delivery: TeamDeliveryMode::NextTurn,
            targets: Some(Vec::new()),
            receipts: BTreeMap::new(),
            published: true,
            error: None,
            external_url: None,
        },
    )
}

#[test]
fn board_delivery_uses_acknowledged_append_order_and_preserves_direct_mail_order() {
    // IDs arrive in the database's lexical order; the two replies were accepted
    // in the opposite order. A third entry arrived after the worker snapshot.
    let original = vec![
        record("a-direct", TeamMessageKind::Request),
        record("b-second-reply", TeamMessageKind::Board),
        record("c-direct", TeamMessageKind::Reply),
        record("d-not-synchronized", TeamMessageKind::Board),
        record("e-first-reply", TeamMessageKind::Board),
        record("f-direct", TeamMessageKind::Note),
    ];
    let original_messages = original
        .iter()
        .map(|(id, record)| (id.clone(), record.message.clone()))
        .collect::<HashMap<_, _>>();
    let synchronized = HashMap::from([("b-second-reply".into(), 12), ("e-first-reply".into(), 11)]);
    let delivered = ordered_delivery_records(original, &synchronized);
    assert_eq!(
        delivered
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "a-direct",
            "e-first-reply",
            "c-direct",
            "b-second-reply",
            "f-direct",
        ]
    );
    for (id, record) in delivered {
        assert_eq!(record.message, original_messages[&id]);
    }
}
