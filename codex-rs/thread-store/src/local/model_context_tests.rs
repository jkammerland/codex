use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::ThreadStore;
use crate::local::rollout_lineage::RolloutLineage;
use crate::local::rollout_lineage::RolloutLineageSegment;
use crate::local::test_support::test_config;
use crate::local::test_support::write_session_file_with_history_mode;

#[tokio::test]
async fn loads_latest_checkpoint_with_required_turn_metadata() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1001);
    let thread_id = codex_protocol::ThreadId::from_string(&uuid.to_string()).expect("thread id");
    write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-00",
        uuid,
        [
            turn_started("turn-1"),
            user_message("older turn"),
            completed_user_message("turn-1", "older turn"),
            turn_context(home.path(), "turn-1"),
            compacted("older checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
            turn_started("turn-2"),
            user_message("latest turn"),
            completed_user_message("turn-2", "latest turn"),
            turn_context(home.path(), "turn-2"),
            compacted("latest checkpoint", Some(Vec::new())),
            turn_complete("turn-2"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(matches!(
        context.items.first(),
        Some(RolloutItem::SessionMeta(_))
    ));
    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::Compacted(compacted) if compacted.message == "latest checkpoint")
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(item, RolloutItem::Compacted(compacted) if compacted.message == "older checkpoint")
    }));
    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-2"))
    }));
}

#[tokio::test]
async fn multi_agent_v2_scan_preserves_durable_mail_behind_a_checkpoint() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1008);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "pending durable mail".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(ResponseItemId::with_suffix("msg", "pending"));
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-07",
        uuid,
        [
            RolloutItem::DurableInterAgentCommunication {
                communication: communication.clone(),
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                turn_id: "mail-turn".to_string(),
                compatibility_shadow: false,
            },
            turn_started("turn-1"),
            user_message("newer turn"),
            completed_user_message("turn-1", "newer turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
        ],
    );
    set_multi_agent_version(path.as_path(), MultiAgentVersion::V2);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::DurableInterAgentCommunication {
                communication: recorded,
                ..
            } if recorded == &communication
        )
    }));
}

#[tokio::test]
async fn multi_agent_v2_model_context_scan_bounds_rejected_physical_records() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1010);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = write_paginated_rollout(home.path(), "2025-01-03T13-00-09", uuid, []);
    let file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open rollout for malformed suffix");
    let mut writer = BufWriter::new(file);
    for _ in 0..=codex_rollout::MAX_PRESERVED_DURABLE_SCAN_ITEMS {
        writeln!(writer, "not-json").expect("append rejected rollout record");
    }
    writer.flush().expect("flush malformed suffix");
    set_multi_agent_version(path.as_path(), MultiAgentVersion::V2);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let error = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect_err("rejected physical records must not permit an unbounded reverse scan");

    assert!(error.to_string().contains("record limit"));
}

#[tokio::test]
async fn multi_agent_v2_physical_record_limit_spans_the_entire_lineage() {
    let home = TempDir::new().expect("temp dir");
    let parent_uuid = Uuid::from_u128(/*v*/ 1012);
    let child_uuid = Uuid::from_u128(/*v*/ 1013);
    let parent_id = ThreadId::from_string(&parent_uuid.to_string()).expect("parent id");
    let child_id = ThreadId::from_string(&child_uuid.to_string()).expect("child id");
    let parent_path = write_paginated_rollout(home.path(), "2025-01-03T13-00-11", parent_uuid, []);
    let child_path = write_paginated_rollout(home.path(), "2025-01-03T13-00-12", child_uuid, []);
    let records_per_segment = codex_rollout::MAX_PRESERVED_DURABLE_SCAN_ITEMS / 2;
    for path in [&parent_path, &child_path] {
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open rollout for malformed suffix");
        let mut writer = BufWriter::new(file);
        for _ in 0..records_per_segment {
            writeln!(writer, "not-json").expect("append rejected rollout record");
        }
        writer.flush().expect("flush malformed suffix");
    }
    set_multi_agent_version(child_path.as_path(), MultiAgentVersion::V2);
    let session_meta = codex_rollout::read_session_meta_line(child_path.as_path())
        .await
        .expect("read child session metadata");
    let lineage = RolloutLineage {
        segments: vec![
            RolloutLineageSegment {
                rollout_id: parent_id,
                rollout_path: parent_path,
                start_ordinal: 0,
                end: None,
            },
            RolloutLineageSegment {
                rollout_id: child_id,
                rollout_path: child_path,
                start_ordinal: 0,
                end: None,
            },
        ],
    };

    let error = scan_model_context_from_lineage_blocking(&lineage, session_meta)
        .expect_err("the physical record budget must not reset at lineage boundaries");

    assert!(error.to_string().contains("record limit"));
}

#[tokio::test]
async fn non_v2_model_context_scan_bounds_rejected_physical_records() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1011);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = write_paginated_rollout(home.path(), "2025-01-03T13-00-10", uuid, []);
    let file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open rollout for malformed suffix");
    let mut writer = BufWriter::new(file);
    for _ in 0..=codex_rollout::MAX_PRESERVED_DURABLE_SCAN_ITEMS {
        writeln!(writer, "not-json").expect("append rejected rollout record");
    }
    writer.flush().expect("flush malformed suffix");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let error = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect_err("ordinary paginated history must also bound malformed physical records");

    assert!(error.to_string().contains("record limit"));
}

#[tokio::test]
async fn multi_agent_v2_scan_compacts_consumed_mail_behind_a_checkpoint() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1009);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "already consumed durable mail".to_string(),
        /*trigger_turn*/ true,
    );
    let communication_id = ResponseItemId::with_suffix("msg", "consumed");
    let previously_compacted_id = ResponseItemId::with_suffix("msg", "previously-compacted");
    communication.id = Some(communication_id.clone());
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-08",
        uuid,
        [
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
                mail_fingerprint: Some("previous-fingerprint".to_string()),
                delivery_id: None,
                terminal_turn_id: None,
                terminal_status: None,
                mail_communication: None,
                mail_start_options: None,
                consumed_mail_id: Some(previously_compacted_id.clone()),
            },
            RolloutItem::DurableInterAgentCommunication {
                communication: communication.clone(),
                start_options: TurnStartOptions::default(),
                fingerprint: "fingerprint".to_string(),
                turn_id: "mail-turn".to_string(),
                compatibility_shadow: false,
            },
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id: communication_id.clone(),
                communication: Some(communication),
                delivered_response: None,
                fingerprint: "fingerprint".to_string(),
                turn_id: "mail-turn".to_string(),
            },
            turn_started("mail-turn"),
            turn_complete("mail-turn"),
            turn_started("turn-1"),
            user_message("newer turn"),
            completed_user_message("turn-1", "newer turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
        ],
    );
    set_multi_agent_version(path.as_path(), MultiAgentVersion::V2);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::InterAgentCommunicationMetadata {
                consumed_mail_id: Some(id),
                mail_fingerprint: Some(fingerprint),
                ..
            } if id == &communication_id && fingerprint == "fingerprint"
        )
    }));
    assert!(context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::InterAgentCommunicationMetadata {
                consumed_mail_id: Some(id),
                mail_fingerprint: Some(fingerprint),
                ..
            } if id == &previously_compacted_id && fingerprint == "previous-fingerprint"
        )
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication: Some(_),
                ..
            }
        )
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::DurableInterAgentCommunication {
                communication,
                ..
            } if communication.id.as_ref() == Some(&communication_id)
        )
    }));
}

#[tokio::test]
async fn multi_agent_v2_scan_keeps_a_suffix_delivery_authoritative() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1014);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let id = ResponseItemId::with_suffix("msg", "suffix-delivery");
    let mut old = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "old pending body".to_string(),
        /*trigger_turn*/ true,
    );
    old.id = Some(id.clone());
    let mut delivered = old.clone();
    delivered.content = "delivered body".to_string();
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-13",
        uuid,
        [
            RolloutItem::DurableInterAgentCommunication {
                communication: old,
                start_options: TurnStartOptions::default(),
                fingerprint: "old-fingerprint".to_string(),
                turn_id: "mail-turn".to_string(),
                compatibility_shadow: false,
            },
            turn_started("turn-1"),
            user_message("newer turn"),
            completed_user_message("turn-1", "newer turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
            RolloutItem::DurableInterAgentCommunicationClaimed {
                communication_id: id.clone(),
                communication: Some(delivered.clone()),
                delivered_response: Some(delivered.to_model_input_item().into()),
                fingerprint: "delivered-fingerprint".to_string(),
                turn_id: "suffix-turn".to_string(),
            },
        ],
    );
    set_multi_agent_version(path.as_path(), MultiAgentVersion::V2);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert_eq!(
        context
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::DurableInterAgentCommunicationClaimed {
                        communication_id,
                        delivered_response: Some(_),
                        ..
                    } if communication_id == &id
                )
            })
            .count(),
        1
    );
    assert!(!context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::InterAgentCommunicationMetadata {
                consumed_mail_id: Some(consumed_id),
                ..
            } if consumed_id == &id
        )
    }));
}

#[tokio::test]
async fn multi_agent_v2_scan_recovers_fingerprintless_mcp11_receipts() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1015);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let id = ResponseItemId::with_suffix("msg", "mcp11-receipt");
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "historical payload".to_string(),
        /*trigger_turn*/ true,
    );
    communication.id = Some(id.clone());
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-14",
        uuid,
        [
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: true,
                mail_fingerprint: Some("historical-fingerprint".to_string()),
                delivery_id: None,
                terminal_turn_id: None,
                terminal_status: None,
                mail_communication: Some(communication),
                mail_start_options: Some(TurnStartOptions::default()),
                consumed_mail_id: None,
            },
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
                mail_fingerprint: None,
                delivery_id: None,
                terminal_turn_id: None,
                terminal_status: None,
                mail_communication: None,
                mail_start_options: None,
                consumed_mail_id: Some(id.clone()),
            },
            turn_started("turn-1"),
            user_message("newer turn"),
            completed_user_message("turn-1", "newer turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
        ],
    );
    set_multi_agent_version(path.as_path(), MultiAgentVersion::V2);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::InterAgentCommunicationMetadata {
                mail_fingerprint: Some(fingerprint),
                mail_communication: None,
                consumed_mail_id: Some(consumed_id),
                ..
            } if consumed_id == &id && fingerprint == "historical-fingerprint"
        )
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::InterAgentCommunicationMetadata {
                mail_communication: Some(communication),
                ..
            } if communication.id.as_ref() == Some(&id)
        )
    }));
}

#[tokio::test]
async fn fork_context_excludes_items_after_frozen_cutoff() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1007);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-06",
        uuid,
        [turn_started("frozen-turn"), user_message("frozen message")],
    );
    let history_base =
        history_position(path.as_path(), thread_id, /*end_ordinal_exclusive*/ 3);
    append_items(path.as_path(), [user_message("later message")]);
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let lineage = store
        .resolve_rollout_lineage(thread_id)
        .await
        .expect("resolve source lineage");
    let session_meta = codex_rollout::read_session_meta_line(path.as_path())
        .await
        .expect("read source metadata");

    let context = load_for_fork(lineage, Some(history_base))
        .await
        .expect("load frozen fork context");

    let expected = vec![
        RolloutItem::SessionMeta(session_meta),
        turn_started("frozen-turn"),
        user_message("frozen message"),
    ];
    assert_eq!(
        serde_json::to_value(context).expect("serialize fork context"),
        serde_json::to_value(expected).expect("serialize expected fork context")
    );
}

#[tokio::test]
async fn loads_turn_metadata_across_an_older_checkpoint() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1006);
    let thread_id = codex_protocol::ThreadId::from_string(&uuid.to_string()).expect("thread id");
    write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-05",
        uuid,
        [
            turn_started("turn-0"),
            user_message("oldest turn"),
            completed_user_message("turn-0", "oldest turn"),
            turn_context(home.path(), "turn-0"),
            turn_complete("turn-0"),
            turn_started("turn-1"),
            user_message("metadata turn"),
            completed_user_message("turn-1", "metadata turn"),
            turn_context(home.path(), "turn-1"),
            compacted("older checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
            turn_started("turn-2"),
            compacted("latest checkpoint", Some(Vec::new())),
            turn_complete("turn-2"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::Compacted(compacted) if compacted.message == "latest checkpoint")
    }));
    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-1"))
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-0"))
    }));
}

#[tokio::test]
async fn returns_scanned_full_history_for_unsupported_compaction() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1002);
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-01",
        uuid,
        [
            turn_started("turn-1"),
            user_message("turn"),
            completed_user_message("turn-1", "turn"),
            turn_context(home.path(), "turn-1"),
            compacted("usable checkpoint", Some(Vec::new())),
            compacted("legacy checkpoint", /*replacement_history*/ None),
            turn_complete("turn-1"),
        ],
    );

    assert_reverse_scan_matches_full_history(home.path(), path.as_path()).await;
}

#[tokio::test]
async fn returns_scanned_full_history_at_bof_without_checkpoint() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1003);
    let path = write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-02",
        uuid,
        [
            turn_started("turn-1"),
            user_message("turn"),
            completed_user_message("turn-1", "turn"),
            turn_context(home.path(), "turn-1"),
            turn_complete("turn-1"),
        ],
    );

    assert_reverse_scan_matches_full_history(home.path(), path.as_path()).await;
}

#[tokio::test]
async fn uses_agent_message_turn_context_without_scanning_older_turn() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1004);
    let thread_id = codex_protocol::ThreadId::from_string(&uuid.to_string()).expect("thread id");
    write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-03",
        uuid,
        [
            turn_started("turn-1"),
            user_message("older turn"),
            completed_user_message("turn-1", "older turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
            turn_started("turn-2"),
            turn_context(home.path(), "turn-2"),
            agent_message("child done"),
            turn_complete("turn-2"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-2"))
    }));
    assert!(!context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-1"))
    }));
}

#[tokio::test]
async fn ignores_contextual_user_messages_when_selecting_turn_context() {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::from_u128(/*v*/ 1005);
    let thread_id = codex_protocol::ThreadId::from_string(&uuid.to_string()).expect("thread id");
    write_paginated_rollout(
        home.path(),
        "2025-01-03T13-00-04",
        uuid,
        [
            turn_started("turn-1"),
            user_message("real user turn"),
            completed_user_message("turn-1", "real user turn"),
            turn_context(home.path(), "turn-1"),
            compacted("checkpoint", Some(Vec::new())),
            turn_complete("turn-1"),
            turn_started("turn-2"),
            contextual_user_message(),
            turn_context(home.path(), "turn-2"),
            turn_complete("turn-2"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await
        .expect("load model context");

    assert!(context.items.iter().any(|item| {
        matches!(item, RolloutItem::TurnContext(context) if context.turn_id.as_deref() == Some("turn-1"))
    }));
}

#[tokio::test]
async fn replays_nested_archived_lineage_from_frozen_prefix() {
    let home = TempDir::new().expect("temp dir");
    let root_uuid = Uuid::from_u128(/*v*/ 2001);
    let root_id = ThreadId::from_string(&root_uuid.to_string()).expect("root id");
    let root_path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-01-00",
        root_uuid,
        [
            user_message("root before checkpoint"),
            compacted("root checkpoint", Some(Vec::new())),
            turn_started("root-excluded"),
            user_message("root after cutoff"),
        ],
    );
    let archived_root = home
        .path()
        .join("archived_sessions")
        .join(root_path.file_name().expect("root filename"));
    std::fs::create_dir_all(archived_root.parent().expect("archive parent"))
        .expect("create archive directory");
    std::fs::rename(root_path, &archived_root).expect("archive root rollout");

    let middle_uuid = Uuid::from_u128(/*v*/ 2002);
    let middle_id = ThreadId::from_string(&middle_uuid.to_string()).expect("middle id");
    let middle_path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-01-01",
        middle_uuid,
        [
            turn_started("middle-turn"),
            user_message("middle inherited"),
            completed_user_message("middle-turn", "middle inherited"),
            turn_context(home.path(), "middle-turn"),
            turn_complete("middle-turn"),
        ],
    );
    set_history_base(
        middle_path.as_path(),
        history_position(
            archived_root.as_path(),
            root_id,
            /*end_ordinal_exclusive*/ 3,
        ),
    );

    let child_uuid = Uuid::from_u128(/*v*/ 2003);
    let child_id = ThreadId::from_string(&child_uuid.to_string()).expect("child id");
    let child_path = write_ordinaled_paginated_rollout(
        home.path(),
        "2025-01-03T13-01-02",
        child_uuid,
        [
            turn_started("child-turn"),
            user_message("child local"),
            completed_user_message("child-turn", "child local"),
            turn_context(home.path(), "child-turn"),
            turn_complete("child-turn"),
        ],
    );
    set_history_base(
        child_path.as_path(),
        history_position(
            middle_path.as_path(),
            middle_id,
            /*end_ordinal_exclusive*/ 6,
        ),
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: child_id,
            include_archived: false,
        })
        .await
        .expect("load lineage model context");

    assert!(matches!(
        context.items.first(),
        Some(RolloutItem::SessionMeta(meta)) if meta.meta.id == child_id
    ));
    let child_meta = codex_rollout::read_session_meta_line(child_path.as_path())
        .await
        .expect("read child metadata");
    let expected = vec![
        RolloutItem::SessionMeta(child_meta),
        compacted("root checkpoint", Some(Vec::new())),
        turn_started("middle-turn"),
        user_message("middle inherited"),
        completed_user_message("middle-turn", "middle inherited"),
        turn_context(home.path(), "middle-turn"),
        turn_complete("middle-turn"),
        turn_started("child-turn"),
        user_message("child local"),
        completed_user_message("child-turn", "child local"),
        turn_context(home.path(), "child-turn"),
        turn_complete("child-turn"),
    ];
    assert_eq!(
        serde_json::to_value(&context.items).expect("serialize context"),
        serde_json::to_value(&expected).expect("serialize expected context")
    );
    // The same frozen lineage must replay from compressed files, without materializing or
    // accidentally including the archived root's records after the inherited cutoff.
    for path in [&archived_root, &middle_path, &child_path] {
        let input = std::fs::File::open(path).expect("open rollout");
        let output = std::fs::File::create(path.with_extension("jsonl.zst"))
            .expect("create compressed rollout");
        zstd::stream::copy_encode(input, output, /*level*/ 3).expect("compress rollout");
        std::fs::remove_file(path).expect("remove plain rollout");
    }
    let compressed_context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: child_id,
            include_archived: false,
        })
        .await
        .expect("load compressed lineage model context");
    assert_eq!(
        serde_json::to_value(compressed_context.items).expect("serialize compressed context"),
        serde_json::to_value(expected).expect("serialize expected context")
    );
    assert!(
        [archived_root, middle_path, child_path]
            .iter()
            .all(|path| !path.exists())
    );
}

fn write_paginated_rollout<const N: usize>(
    home: &Path,
    timestamp: &str,
    uuid: Uuid,
    items: [RolloutItem; N],
) -> PathBuf {
    let path =
        write_session_file_with_history_mode(home, timestamp, uuid, ThreadHistoryMode::Paginated)
            .expect("write session file");
    append_items(path.as_path(), items);
    path
}

fn write_ordinaled_paginated_rollout<const N: usize>(
    home: &Path,
    timestamp: &str,
    uuid: Uuid,
    items: [RolloutItem; N],
) -> PathBuf {
    let path =
        write_session_file_with_history_mode(home, timestamp, uuid, ThreadHistoryMode::Paginated)
            .expect("write session file");
    let mut file = OpenOptions::new()
        .append(true)
        .open(path.as_path())
        .expect("open session file");
    for (index, item) in items.into_iter().enumerate() {
        let line = RolloutLine {
            timestamp: "2025-01-03T13:00:01Z".to_string(),
            ordinal: Some(u64::try_from(index).expect("fixture index fits u64") + 1),
            item,
        };
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("serialize line")
        )
        .expect("append rollout line");
    }
    path
}

fn set_history_base(path: &Path, history_base: HistoryPosition) {
    let contents = std::fs::read_to_string(path).expect("read rollout");
    let mut lines = contents.lines();
    let mut head: serde_json::Value =
        serde_json::from_str(lines.next().expect("session meta line")).expect("parse head");
    head["payload"]["history_base"] =
        serde_json::to_value(history_base).expect("serialize history base");
    let mut updated = serde_json::to_string(&head).expect("serialize head");
    for line in lines {
        updated.push('\n');
        updated.push_str(line);
    }
    updated.push('\n');
    std::fs::write(path, updated).expect("write history base");
}

fn set_multi_agent_version(path: &Path, version: MultiAgentVersion) {
    let contents = std::fs::read_to_string(path).expect("read rollout");
    let mut lines = contents.lines();
    let mut head: serde_json::Value =
        serde_json::from_str(lines.next().expect("session meta line")).expect("parse head");
    head["payload"]["multi_agent_version"] =
        serde_json::to_value(version).expect("serialize multi-agent version");
    let mut updated = serde_json::to_string(&head).expect("serialize head");
    for line in lines {
        updated.push('\n');
        updated.push_str(line);
    }
    updated.push('\n');
    std::fs::write(path, updated).expect("write multi-agent version");
}

fn history_position(
    path: &Path,
    thread_id: ThreadId,
    end_ordinal_exclusive: u64,
) -> HistoryPosition {
    HistoryPosition {
        thread_id,
        end_ordinal_exclusive,
        end_byte_offset: rollout_end_byte_offset(path, end_ordinal_exclusive),
    }
}

fn rollout_end_byte_offset(path: &Path, end_ordinal_exclusive: u64) -> u64 {
    let contents = std::fs::read(path).expect("read rollout");
    let mut byte_offset = 0_u64;
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let parsed: RolloutLine =
            serde_json::from_slice(line).expect("parse rollout line for byte offset");
        if parsed.ordinal == Some(end_ordinal_exclusive) {
            return byte_offset;
        }
        byte_offset += u64::try_from(line.len()).expect("line length fits u64");
    }
    byte_offset
}

async fn assert_reverse_scan_matches_full_history(home: &Path, path: &Path) {
    let session_meta = codex_rollout::read_session_meta_line(path)
        .await
        .expect("read session metadata");
    let store = LocalThreadStore::new(test_config(home), /*state_db*/ None);
    let items = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: session_meta.meta.id,
            include_archived: false,
        })
        .await
        .expect("scan model context")
        .items;
    let full_items = read_thread::load_history_items(path)
        .await
        .expect("load full history");

    assert_eq!(
        serde_json::to_value(items).expect("serialize scanned items"),
        serde_json::to_value(full_items).expect("serialize full items")
    );
}

fn append_items<const N: usize>(path: &Path, items: [RolloutItem; N]) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open session file");
    for item in items {
        let line = RolloutLine {
            timestamp: "2025-01-03T13:00:01Z".to_string(),
            ordinal: None,
            item,
        };
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("serialize line")
        )
        .expect("append rollout line");
    }
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: Some(128_000),
        collaboration_mode_kind: Default::default(),
    }))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn user_message(message: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: message.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn contextual_user_message() -> RolloutItem {
    user_message("<environment_context>context only</environment_context>")
}

fn completed_user_message(turn_id: &str, message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000000")
            .expect("fixture thread id"),
        turn_id: turn_id.to_string(),
        item: TurnItem::UserMessage(UserMessageItem {
            id: format!("user-{turn_id}"),
            client_id: None,
            content: vec![UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }],
        }),
        started_at_ms: Some(0),
        completed_at_ms: 0,
    }))
}

fn agent_message(message: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: None,
            author: "worker".to_string(),
            recipient: "root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: message.to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn turn_context(root: &Path, turn_id: &str) -> RolloutItem {
    RolloutItem::TurnContext(TurnContextItem {
        turn_id: Some(turn_id.to_string()),
        cwd: serde_json::from_value(serde_json::json!(root)).expect("absolute cwd"),
        workspace_roots: None,
        current_date: None,
        timezone: None,
        approval_policy: AskForApproval::Never,
        approvals_reviewer: None,
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "test-model".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: None,
        cyber_access_program: None,
        effort: None,
        summary: ReasoningSummary::Auto,
    })
}

fn compacted(message: &str, replacement_history: Option<Vec<ResponseItem>>) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: message.to_string(),
        replacement_history: replacement_history
            .map(|items| items.into_iter().map(Into::into).collect()),
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    })
}
