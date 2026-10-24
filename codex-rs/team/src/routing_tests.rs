use std::collections::BTreeMap;
use std::collections::HashMap;

use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::ordered_delivery_records;
use crate::coordinator::Coordinator;
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
            abandonments: Default::default(),
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

#[tokio::test]
async fn external_reply_destinations_require_operator_identity_and_gitea_provenance()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(directory.path())).await?;
    let sender = crate::tests::agent(&coordinator, "reviewer").await?;
    let original = coordinator
        .operator_message(
            Some(sender.clone()),
            "Please review".into(),
            TeamMessageKind::Request,
            /*reply_to*/ None,
            /*topic*/ None,
        )
        .await?;
    let mut reply = TeamMessage {
        id: uuid::Uuid::new_v4().to_string(),
        sender: sender.clone(),
        recipient: Some(original.sender.clone()),
        kind: TeamMessageKind::Reply,
        body: "Reviewed".into(),
        reply_to: Some(original.id.clone()),
        topic: None,
        created_at: 1,
    };

    // An operator destination is tied to this coordinator and its exact inbox.
    assert_eq!(
        coordinator
            .prepare_message(reply.clone(), TeamDeliveryMode::NextTurn)
            .await?
            .message,
        reply
    );
    for recipient in [
        TeamAddress {
            server_id: uuid::Uuid::new_v4().to_string(),
            ..original.sender.clone()
        },
        TeamAddress {
            thread_id: "other".into(),
            ..original.sender
        },
    ] {
        let mut invalid = reply.clone();
        invalid.recipient = Some(recipient);
        assert_eq!(
            coordinator
                .prepare_message(invalid, TeamDeliveryMode::NextTurn)
                .await
                .unwrap_err()
                .to_string(),
            "unknown operator inbox"
        );
    }

    // A Gitea reply must retain its originating conversation and recipient.
    let gitea = TeamAddress {
        machine_id: "gitea".into(),
        server_id: "example/project".into(),
        thread_id: "12".into(),
    };
    let original = TeamMessage {
        sender: gitea.clone(),
        ..original
    };
    coordinator
        .ingest(original.clone(), TeamDeliveryMode::NextTurn)
        .await?;
    reply.recipient = Some(gitea.clone());
    assert_eq!(
        coordinator
            .prepare_message(reply.clone(), TeamDeliveryMode::NextTurn)
            .await
            .unwrap_err()
            .to_string(),
        "reply does not belong to this Gitea conversation"
    );
    coordinator
        .store
        .put(
            "gitea_origin",
            &original.id,
            &crate::gitea::Origin {
                integration: None,
                repository: "example/project".into(),
                number: 12,
                url: "https://example.invalid/project/issues/12".into(),
            },
        )
        .await?;
    assert_eq!(
        coordinator
            .prepare_message(reply.clone(), TeamDeliveryMode::NextTurn)
            .await?
            .message,
        reply
    );

    let mut other_conversation = reply.clone();
    other_conversation.recipient = Some(TeamAddress {
        thread_id: "13".into(),
        ..gitea
    });
    let mut other_agent = reply.clone();
    other_agent.sender = crate::tests::agent(&coordinator, "other").await?;
    let mut non_reply = reply.clone();
    non_reply.kind = TeamMessageKind::Request;
    for invalid in [other_conversation, other_agent, non_reply] {
        assert_eq!(
            coordinator
                .prepare_message(invalid, TeamDeliveryMode::NextTurn)
                .await
                .unwrap_err()
                .to_string(),
            "reply does not belong to this Gitea conversation"
        );
    }
    reply.reply_to = None;
    assert_eq!(
        coordinator
            .prepare_message(reply, TeamDeliveryMode::NextTurn)
            .await
            .unwrap_err()
            .to_string(),
        "Gitea replies require an original message"
    );
    Ok(())
}
