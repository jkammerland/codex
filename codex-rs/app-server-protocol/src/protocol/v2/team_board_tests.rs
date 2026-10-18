use std::collections::BTreeSet;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::*;

fn assert_schema_matches_serialized_variant<T: schemars::JsonSchema>(wire: &Value) {
    let schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    let variants = schema["oneOf"]
        .as_array()
        .expect("tagged enum has variants");
    let variant = variants
        .iter()
        .find(|variant| {
            variant["properties"]["type"]["enum"]
                .as_array()
                .is_some_and(|tags| tags.contains(&wire["type"]))
        })
        .expect("serialized discriminator occurs in schema");
    let serialized_keys = wire
        .as_object()
        .expect("serialized action is object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let schema_keys = variant["properties"]
        .as_object()
        .expect("variant schema is object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(schema_keys, serialized_keys);
    for required in variant["required"]
        .as_array()
        .expect("variant has required fields")
    {
        assert!(serialized_keys.contains(required.as_str().expect("required field is string")));
    }
}

#[test]
fn board_request_schema_and_wire_round_trip_share_camel_case_fields() {
    // These requests exercise thread identity, reply linkage and durable read
    // ordering. SDK schemas must describe the same keys accepted by serde.
    for (request, expected) in [
        (
            TeamBoardParams::Reply {
                conversation_id: "conversation".into(),
                reply_to: Some("entry".into()),
                body: "Reply".into(),
                mentions: Vec::new(),
            },
            json!({"type":"reply", "conversationId":"conversation", "replyTo":"entry", "body":"Reply", "mentions":[]}),
        ),
        (
            TeamBoardParams::MarkRead {
                conversation_id: "conversation".into(),
                identity: Some(TeamBoardIdentity::User),
                append_seq: 42,
            },
            json!({"type":"markRead", "conversationId":"conversation", "identity":{"type":"user"}, "appendSeq":42}),
        ),
    ] {
        let wire = serde_json::to_value(&request).expect("request serializes");
        assert_eq!(wire, expected);
        assert_eq!(
            serde_json::from_value::<TeamBoardParams>(wire.clone()).expect("request deserializes"),
            request
        );
        assert_schema_matches_serialized_variant::<TeamBoardParams>(&wire);
    }
}

#[test]
fn board_response_schema_and_wire_round_trip_share_pagination_and_identity_fields() {
    for (response, expected) in [
        (
            TeamBoardResponse::Topics {
                revision: 7,
                data: vec!["general".into()],
                next_cursor: Some("cursor".into()),
            },
            json!({"type":"topics", "revision":7, "data":["general"], "nextCursor":"cursor"}),
        ),
        (
            TeamBoardResponse::Mutation {
                revision: 8,
                state: TeamBoardMutationState::Applied,
                id: "operation".into(),
                conversation_id: Some("conversation".into()),
            },
            json!({"type":"mutation", "revision":8, "state":"applied", "id":"operation", "conversationId":"conversation"}),
        ),
    ] {
        let wire = serde_json::to_value(&response).expect("response serializes");
        assert_eq!(wire, expected);
        assert_eq!(
            serde_json::from_value::<TeamBoardResponse>(wire.clone())
                .expect("response deserializes"),
            response
        );
        assert_schema_matches_serialized_variant::<TeamBoardResponse>(&wire);
    }
}

#[test]
fn legacy_follow_wire_forms_keep_their_original_operation_digest_input() {
    for wire in [
        json!({"type":"follow", "conversationId":"root", "identity":null, "enabled":true}),
        json!({"type":"topicFollow", "topic":"review", "identity":null, "enabled":false}),
    ] {
        let request: TeamBoardParams = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), wire);
    }
    let request = TeamBoardParams::SubscriptionSet {
        target: TeamBoardSubscriptionTarget::Conversation {
            conversation_id: "root".into(),
        },
        identity: None,
        mode: TeamBoardSubscriptionMode::Watch,
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(
        wire,
        json!({"type":"subscriptionSet", "target":{"type":"conversation", "conversationId":"root"}, "identity":null, "mode":"watch"})
    );
    assert_eq!(
        serde_json::from_value::<TeamBoardParams>(wire.clone()).unwrap(),
        request
    );
    assert_schema_matches_serialized_variant::<TeamBoardParams>(&wire);
}
