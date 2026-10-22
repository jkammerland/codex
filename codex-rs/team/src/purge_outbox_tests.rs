use anyhow::Result;
use codex_app_server_protocol::*;
use codex_state::SqliteConfig;
use codex_team_store::TeamStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

use crate::coordinator::Coordinator;
use crate::model::*;

async fn fixture() -> Result<(
    tempfile::TempDir,
    std::sync::Arc<Coordinator>,
    TeamAddress,
    crate::Machine,
    crate::Server,
    MessageRecord,
)> {
    let home = tempfile::tempdir()?;
    let coordinator = Coordinator::open(crate::tests::manifest(home.path())).await?;
    let address = crate::tests::agent(&coordinator, "Worker").await?;
    let server = crate::Server {
        id: address.server_id.clone(),
        label: "Account".into(),
        endpoint: crate::Endpoint::Local {
            socket: home.path().join("unused.sock"),
        },
        cancellation_adapters: Vec::new(),
        environment_capabilities: Default::default(),
    };
    let machine = crate::Machine {
        id: address.machine_id.clone(),
        label: "Worker".into(),
        servers: vec![server.clone()],
    };
    coordinator
        .store
        .put(
            "installation",
            &server_key(&machine.id, &server.id),
            &"installation",
        )
        .await?;
    let TeamResponse::Message { mut message } = coordinator
        .execute(TeamRequest::Post {
            topic: "testing".into(),
            body: "removed root body".into(),
        })
        .await?
    else {
        panic!("post")
    };
    message.targets = Some(vec![address.clone()]);
    coordinator
        .store
        .put("message", &message.message.id, &message)
        .await?;
    Ok((home, coordinator, address, machine, server, *message))
}

async fn purge(coordinator: &Coordinator) -> Result<Vec<TeamBoardPurgeJournal>> {
    coordinator
        .execute_operation(TeamOperation::new(TeamRequest::BoardPurge {
            scope: TeamBoardPurgeScope::All,
            expected_revision: coordinator.store.board_clock().await?.revision,
        }))
        .await?;
    coordinator.store.board_purge_journal().await
}

#[tokio::test]
async fn lost_receipt_ack_does_not_replay_scrubbed_content_under_the_original_identity()
-> Result<()> {
    let (home, coordinator, address, machine, server, record) = fixture().await?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(home.path())?);
    let worker = TeamStore::open(&sqlite, &home.path().join("worker.sqlite")).await?;
    let delivery = TeamDelivery {
        id: format!("{}:{}", record.message.id, address_key(&address)),
        thread_id: address.thread_id.clone(),
        message: record.message.clone(),
        mode: TeamDeliveryMode::NextTurn,
        wake: true,
    };
    let mut receipt = worker.receive(&delivery).await?;
    receipt.state = TeamReceiptState::Accepted;
    receipt.turn_id = Some("admitted-turn".into());
    receipt.detail = Some("private receipt diagnostic".into());
    worker.update_receipt(&receipt).await?;
    let old = worker.outbox(0, 10).await?.remove(0);
    let old_entry = TeamOutboxEntry {
        sequence: old.sequence,
        id: old.id.clone(),
        kind: old.kind,
        payload: old.payload,
    };
    coordinator
        .accept_outbox(&machine, &server, &old_entry)
        .await?;
    // The coordinator committed the receipt, but its ACK never reached worker.
    let intake_before = coordinator
        .store
        .get::<String>("outbox_intake", &format!("installation/{}", old.id))
        .await?
        .unwrap();
    worker
        .apply_board_purges(&purge(&coordinator).await?)
        .await?;
    let pending = worker.outbox(0, 10).await?;
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].id, old.id);
    let sanitized: TeamReceipt = serde_json::from_value(pending[0].payload.clone())?;
    assert_eq!(
        sanitized,
        TeamReceipt {
            detail: None,
            ..receipt.clone()
        }
    );
    for entry in pending {
        coordinator
            .accept_outbox(
                &machine,
                &server,
                &TeamOutboxEntry {
                    sequence: entry.sequence,
                    id: entry.id,
                    kind: entry.kind,
                    payload: entry.payload,
                },
            )
            .await?;
        worker.acknowledge(&[entry.sequence]).await?;
    }
    assert!(worker.outbox(0, 10).await?.is_empty());
    assert_eq!(
        coordinator
            .store
            .get::<String>("outbox_intake", &format!("installation/{}", old.id))
            .await?,
        Some(intake_before)
    );
    // A delayed original packet remains a valid duplicate. Completion may
    // arrive after purge too, with state preserved and diagnostics scrubbed.
    coordinator
        .accept_outbox(&machine, &server, &old_entry)
        .await?;
    receipt.state = TeamReceiptState::Completed;
    worker.update_receipt(&receipt).await?;
    let completion = worker.outbox(0, 10).await?.remove(0);
    assert_eq!(
        serde_json::from_value::<TeamReceipt>(completion.payload.clone())?,
        TeamReceipt {
            detail: None,
            ..receipt.clone()
        }
    );
    let mut stale_completion = receipt.clone();
    stale_completion.detail = Some("late private diagnostic".into());
    coordinator
        .accept_outbox(
            &machine,
            &server,
            &TeamOutboxEntry {
                sequence: completion.sequence,
                id: completion.id,
                kind: completion.kind,
                payload: serde_json::to_value(stale_completion)?,
            },
        )
        .await?;
    let final_record = coordinator
        .store
        .get::<MessageRecord>("message", &record.message.id)
        .await?
        .unwrap();
    assert_eq!(
        final_record.receipts[&delivery.id],
        TeamReceipt {
            detail: None,
            ..receipt
        }
    );
    assert!(final_record.message.body.is_empty());
    Ok(())
}

#[tokio::test]
async fn late_rejected_descendants_are_scrubbed_for_both_arrival_orders() -> Result<()> {
    for reverse in [false, true] {
        let (_home, coordinator, address, machine, server, record) = fixture().await?;
        purge(&coordinator).await?;
        let first = Uuid::new_v4().to_string();
        let second = Uuid::new_v4().to_string();
        let third = Uuid::new_v4().to_string();
        let mut entries = Vec::new();
        for (sequence, (id, parent)) in [
            (&first, &record.message.id),
            (&second, &first),
            (&third, &second),
        ]
        .into_iter()
        .enumerate()
        {
            let mut message = record.message.clone();
            message.id = id.clone();
            message.reply_to = Some(parent.clone());
            message.sender = address.clone();
            message.body = format!("private disconnected body {id}");
            entries.push(TeamOutboxEntry {
                sequence: sequence as i64 + 1,
                id: id.clone(),
                kind: "message".into(),
                payload: json!({"message":message,"delivery":"nextTurn"}),
            });
        }
        if reverse {
            entries.reverse();
        }
        for entry in &entries {
            coordinator.accept_outbox(&machine, &server, entry).await?;
        }
        let rejected = coordinator
            .store
            .list::<Value>("rejected_outbox", "", u32::MAX)
            .await?;
        assert_eq!(rejected.len(), 3);
        assert!(!serde_json::to_string(&rejected)?.contains("private disconnected body"));
        for id in [&first, &second, &third] {
            assert!(coordinator.store.board_message_removed(id).await?);
        }
        // Immutable intake digests still recognize the original packets.
        for entry in &entries {
            coordinator.accept_outbox(&machine, &server, entry).await?;
        }
        assert_eq!(
            coordinator
                .store
                .list::<Value>("rejected_outbox", "", u32::MAX)
                .await?,
            rejected
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejected_receipt_diagnostics_are_scrubbed_before_and_after_purge() -> Result<()> {
    let (_home, coordinator, address, machine, server, record) = fixture().await?;
    let receipt = TeamReceipt {
        delivery_id: format!("{}:wrong-recipient", record.message.id),
        thread_id: address.thread_id,
        state: TeamReceiptState::Completed,
        turn_id: Some("completed".into()),
        detail: Some("private rejected receipt diagnostic".into()),
    };
    let original = TeamOutboxEntry {
        sequence: 1,
        id: "original-rejected-receipt".into(),
        kind: "receipt".into(),
        payload: serde_json::to_value(&receipt)?,
    };
    coordinator
        .accept_outbox(&machine, &server, &original)
        .await?;
    let before = coordinator
        .store
        .list::<Value>("rejected_outbox", "", u32::MAX)
        .await?;
    assert!(serde_json::to_string(&before)?.contains("private rejected receipt diagnostic"));
    purge(&coordinator).await?;

    let late = TeamOutboxEntry {
        sequence: 2,
        id: "late-rejected-receipt".into(),
        ..original.clone()
    };
    coordinator.accept_outbox(&machine, &server, &late).await?;
    for entry in [&original, &late] {
        // Diagnostic removal must preserve the original immutable intake hash.
        coordinator.accept_outbox(&machine, &server, entry).await?;
    }
    let rejected = coordinator
        .store
        .list::<Value>("rejected_outbox", "", u32::MAX)
        .await?;
    assert_eq!(rejected.len(), 2);
    for (_, value) in rejected {
        assert_eq!(value["entry"]["kind"], "removed");
        assert_eq!(value["entry"]["payload"], json!({"removed":true}));
        assert_eq!(value["reason"], "Permanently removed from Board.");
    }
    Ok(())
}
