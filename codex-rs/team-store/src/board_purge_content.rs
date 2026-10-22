//! Shared content-free markers and coordinator cleanup within the purge transaction.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::TeamBoardPurgeJournal;
use serde_json::Value;
use serde_json::json;
use sqlx::Sqlite;
use sqlx::Transaction;

pub(super) async fn put_record(
    tx: &mut Transaction<'_, Sqlite>,
    namespace: &str,
    id: &str,
    value: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO record(namespace,id,payload) VALUES (?,?,?) ON CONFLICT(namespace,id) DO UPDATE SET payload = excluded.payload")
        .bind(namespace).bind(id).bind(serde_json::to_string(value)?).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn mark_removed(
    tx: &mut Transaction<'_, Sqlite>,
    journal: &TeamBoardPurgeJournal,
) -> Result<()> {
    for (namespace, ids) in [
        ("board_removed_conversation", &journal.conversation_ids),
        ("board_removed_message", &journal.message_ids),
    ] {
        for id in ids {
            put_record(tx, namespace, id, &json!(journal.operation_id)).await?;
        }
    }
    Ok(())
}

pub(super) fn matches_removed(
    id: &str,
    value: &Value,
    messages: &BTreeSet<String>,
    conversations: &BTreeSet<String>,
) -> bool {
    messages.contains(id)
        || [
            "/message/id",
            "/message/message/id",
            "/entry/id",
            "/entry/payload/message/id",
        ]
        .iter()
        .any(|path| {
            value
                .pointer(path)
                .and_then(Value::as_str)
                .is_some_and(|id| messages.contains(id))
        })
        || [
            "/message/replyTo",
            "/message/message/replyTo",
            "/request/replyTo",
            "/request/conversationId",
            "/entry/payload/message/replyTo",
            "/entry/payload/request/conversationId",
            "/entry/payload/request/replyTo",
        ]
        .iter()
        .any(|path| {
            value
                .pointer(path)
                .and_then(Value::as_str)
                .is_some_and(|id| messages.contains(id) || conversations.contains(id))
        })
        || value
            .pointer("/entry/payload/deliveryId")
            .and_then(Value::as_str)
            .and_then(|id| id.split_once(':'))
            .is_some_and(|(id, _)| messages.contains(id))
}

/// Follow queued reply edges before replacing any payload. Reverse indexing
/// makes the result independent of arrival order without rescanning long chains.
pub(super) fn expand_removed_messages(
    removed: &mut BTreeMap<String, String>,
    rows: &[(String, Value)],
) {
    let mut dependents = BTreeMap::<&str, Vec<usize>>::new();
    for (index, (id, value)) in rows.iter().enumerate() {
        for dependency in std::iter::once(id.as_str()).chain(
            [
                "/message/id",
                "/message/message/id",
                "/entry/id",
                "/entry/payload/message/id",
                "/message/replyTo",
                "/message/message/replyTo",
                "/request/replyTo",
                "/request/conversationId",
                "/entry/payload/message/replyTo",
                "/entry/payload/request/conversationId",
                "/entry/payload/request/replyTo",
            ]
            .into_iter()
            .filter_map(|path| value.pointer(path).and_then(Value::as_str)),
        ) {
            dependents.entry(dependency).or_default().push(index);
        }
    }
    let mut pending = removed.keys().cloned().collect::<VecDeque<_>>();
    let mut visited = BTreeSet::new();
    while let Some(parent) = pending.pop_front() {
        let operation = removed[&parent].clone();
        for &index in dependents.get(parent.as_str()).into_iter().flatten() {
            if !visited.insert(index) {
                continue;
            }
            let (id, value) = &rows[index];
            for id in std::iter::once(id.as_str()).chain(
                [
                    "/message/id",
                    "/message/message/id",
                    "/entry/id",
                    "/entry/payload/message/id",
                ]
                .into_iter()
                .filter_map(|path| value.pointer(path).and_then(Value::as_str)),
            ) {
                if !removed.contains_key(id) {
                    removed.insert(id.to_owned(), operation.clone());
                    pending.push_back(id.to_owned());
                }
            }
        }
    }
}

/// Late child-before-parent rejections can predate discovery of the removed
/// ancestor. Scrub the whole rejected chain in the same transaction as intake.
pub(super) async fn purge_removed_rejected_outboxes(
    tx: &mut Transaction<'_, Sqlite>,
) -> Result<()> {
    let markers: Vec<(String, String)> = sqlx::query_as(
        "SELECT id,payload FROM record WHERE namespace IN ('board_removed_message','board_removed_conversation')",
    ).fetch_all(&mut **tx).await?;
    let mut removed = markers
        .into_iter()
        .map(|(id, operation)| Ok((id, serde_json::from_str::<String>(&operation)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    if removed.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id,payload FROM record WHERE namespace = 'rejected_outbox' AND json_extract(payload, '$.entry.kind') != 'removed'",
    ).fetch_all(&mut **tx).await?;
    let mut rows = rows
        .into_iter()
        .map(|(id, payload)| Ok((id, serde_json::from_str::<Value>(&payload)?)))
        .collect::<Result<Vec<_>>>()?;
    let accepted: Vec<String> =
        sqlx::query_scalar("SELECT id FROM record WHERE namespace = 'message'")
            .fetch_all(&mut **tx)
            .await?;
    let accepted = accepted.into_iter().collect::<BTreeSet<_>>();
    let candidates = rows
        .iter()
        .filter_map(|(_, value)| {
            let id = value.pointer("/entry/id")?.as_str()?;
            // Conflicting rejected envelopes do not own an already accepted ID.
            // Their payload is scrubbed below without removing the accepted post.
            if accepted.contains(id)
                || value
                    .pointer("/entry/payload/message/id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| accepted.contains(id))
            {
                return None;
            }
            Some((id.to_owned(), value.clone()))
        })
        .collect::<Vec<_>>();
    expand_removed_messages(&mut removed, &candidates);
    for (id, operation) in &removed {
        sqlx::query("INSERT INTO record(namespace,id,payload) VALUES ('board_removed_message',?,?) ON CONFLICT(namespace,id) DO NOTHING")
            .bind(id).bind(serde_json::to_string(operation)?).execute(&mut **tx).await?;
    }
    let messages = removed.into_keys().collect();
    for (id, value) in &mut rows {
        if matches_removed(id, value, &messages, &BTreeSet::new()) {
            value["entry"]["kind"] = json!("removed");
            value["entry"]["payload"] = json!({"removed":true});
            value["reason"] = json!("Permanently removed from Board.");
            put_record(tx, "rejected_outbox", id, value).await?;
        }
    }
    Ok(())
}

pub(super) async fn ensure_board_payload_live(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    kind: &str,
    value: &Value,
) -> Result<()> {
    if !matches!(kind, "board" | "message") {
        return Ok(());
    }
    let mut ids = vec![id];
    for pointer in [
        "/message/id",
        "/message/replyTo",
        "/request/replyTo",
        "/request/conversationId",
    ] {
        if let Some(id) = value.pointer(pointer).and_then(Value::as_str) {
            ids.push(id);
        }
    }
    for id in ids {
        let removed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM record WHERE namespace IN ('board_removed_message','board_removed_conversation') AND id = ?)")
            .bind(id).fetch_one(&mut **tx).await?;
        ensure!(!removed, "Board conversation was permanently removed");
    }
    Ok(())
}

pub(super) async fn purge_coordinator_content(
    tx: &mut Transaction<'_, Sqlite>,
    journal: &mut TeamBoardPurgeJournal,
) -> Result<()> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT namespace,id,payload FROM record WHERE namespace IN ('message','operator_result','rejected_outbox')",
    ).fetch_all(&mut **tx).await?;
    let rows = rows
        .into_iter()
        .map(|(namespace, id, payload)| {
            Ok((namespace, id, serde_json::from_str::<Value>(&payload)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let accepted = rows
        .iter()
        .filter(|(namespace, _, _)| namespace == "message")
        .map(|(_, id, _)| id.as_str())
        .collect::<BTreeSet<_>>();
    let candidates = rows
        .iter()
        .filter_map(|(namespace, id, value)| match namespace.as_str() {
            "message" => Some((id.clone(), value.clone())),
            "rejected_outbox" => {
                let id = value.pointer("/entry/id")?.as_str()?;
                if accepted.contains(id)
                    || value
                        .pointer("/entry/payload/message/id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| accepted.contains(id))
                {
                    return None;
                }
                Some((id.to_owned(), value.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut removed = journal
        .message_ids
        .iter()
        .chain(&journal.conversation_ids)
        .map(|id| (id.clone(), journal.operation_id.clone()))
        .collect();
    expand_removed_messages(&mut removed, &candidates);
    journal.message_ids = removed.into_keys().collect();
    mark_removed(tx, journal).await?;
    let messages = journal.message_ids.iter().cloned().collect::<BTreeSet<_>>();
    let conversations = journal
        .conversation_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for id in &journal.message_ids {
        sqlx::query("DELETE FROM record WHERE namespace = 'board_entry' AND id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    for namespace in ["board_follower", "board_read"] {
        for conversation in &journal.conversation_ids {
            sqlx::query("DELETE FROM record WHERE namespace = ? AND json_extract(payload, '$.conversationId') = ?")
                .bind(namespace).bind(conversation).execute(&mut **tx).await?;
        }
    }

    // IDs and delivery evidence remain usable; no retry result retains the body.
    for (namespace, id, mut value) in rows {
        if !matches_removed(&id, &value, &messages, &conversations) {
            continue;
        }
        match namespace.as_str() {
            "message" => {
                value["message"]["body"] = json!("");
                value["error"] = json!("Permanently removed from Board.");
                if let Some(receipts) = value.get_mut("receipts").and_then(Value::as_object_mut) {
                    for receipt in receipts.values_mut() {
                        receipt["detail"] = Value::Null;
                    }
                }
            }
            "operator_result" => {
                let message_id = value
                    .pointer("/message/message/id")
                    .or_else(|| value.pointer("/message/id"))
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned();
                value = json!({"type":"removed","messageId":message_id});
            }
            "rejected_outbox" => {
                value["entry"]["kind"] = json!("removed");
                value["entry"]["payload"] = json!({"removed":true});
                value["reason"] = json!("Permanently removed from Board.");
            }
            _ => unreachable!("query restricts namespaces"),
        }
        put_record(tx, &namespace, &id, &value).await?;
    }
    Ok(())
}
