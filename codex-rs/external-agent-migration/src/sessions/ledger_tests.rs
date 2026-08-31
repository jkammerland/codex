use super::CompletedExternalAgentSessionImport;
use super::ImportedConnectorCandidate;
use super::ImportedExternalAgentSessionLedger;
use super::append_imported_session_connector_names;
use super::begin_existing_session_append;
use super::checkpoint_existing_session_import;
use super::read_imported_connector_candidates;
use super::record_completed_session_imports;
use super::record_detected_session_connectors;
use super::reserve_session_import;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn empty_ledger_does_not_read_source() {
    let root = TempDir::new().expect("tempdir");
    let missing_source = root.path().join("missing-session.jsonl");

    assert!(
        !ImportedExternalAgentSessionLedger::default()
            .contains_current_source(&missing_source)
            .expect("empty ledger cannot contain sources")
    );
}

#[test]
fn completed_imports_do_not_read_source_files() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    let contents = b"session contents";
    std::fs::write(&source_path, contents).expect("source");
    let source_path = std::fs::canonicalize(&source_path).expect("canonical source");
    std::fs::remove_file(&source_path).expect("remove source");
    let imported_thread_id = ThreadId::new();

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: format!("{:x}", Sha256::digest(contents)),
            imported_thread_id,
            connector_names: Vec::new(),
            title: Some("Imported session".to_string()),
        }],
    )
    .expect("record completed imports");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, imported_thread_id);
    assert_eq!(ledger.records[0].source_modified_at, None);
    assert_eq!(ledger.records[0].title.as_deref(), Some("Imported session"));
}

#[test]
fn completed_import_refreshes_existing_record_metadata() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    let contents = b"session contents";
    std::fs::write(&source_path, contents).expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let content_sha256 = format!("{:x}", Sha256::digest(contents));
    let first_thread_id = ThreadId::new();
    let second_thread_id = ThreadId::new();

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: content_sha256.clone(),
            imported_thread_id: first_thread_id,
            connector_names: vec!["Gmail".to_string()],
            title: Some("First title".to_string()),
        }],
    )
    .expect("record first import");
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: content_sha256,
            imported_thread_id: second_thread_id,
            connector_names: vec!["Slack".to_string()],
            title: Some("Second title".to_string()),
        }],
    )
    .expect("record replacement import");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records.len(), 1);
    assert_eq!(ledger.records[0].source_path, source_path);
    assert_eq!(ledger.records[0].imported_thread_id, second_thread_id);
    assert!(ledger.records[0].source_modified_at.is_some());
    assert_eq!(ledger.records[0].connector_names, vec!["Gmail", "Slack"]);
    assert_eq!(ledger.records[0].title.as_deref(), Some("Second title"));
}

#[test]
fn ledger_mutations_wait_for_the_cross_process_write_lock() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "session").expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let mut lock = fd_lock::RwLock::new(
        super::open_import_ledger_lock(&codex_home).expect("open ledger lock"),
    );
    let guard = lock.write().expect("hold ledger lock");
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker_home = codex_home.clone();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).expect("started signal");
        let result = record_completed_session_imports(
            &worker_home,
            vec![CompletedExternalAgentSessionImport {
                source_path,
                source_content_sha256: "content".to_string(),
                imported_thread_id: ThreadId::new(),
                connector_names: Vec::new(),
                title: None,
            }],
        );
        finished_tx.send(result).expect("finished signal");
    });
    started_rx.recv().expect("worker started");

    assert!(matches!(
        finished_rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    finished_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("mutation released after lock")
        .expect("mutation succeeded");
    worker.join().expect("worker joined");
    assert_eq!(
        super::load_import_ledger(&codex_home)
            .expect("ledger")
            .records
            .len(),
        1
    );
}

#[test]
fn append_checkpoint_is_two_phase_compare_and_swap_and_preserves_record_metadata() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "old").expect("initial source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let thread_id = ThreadId::new();
    let old_hash = format!("{:x}", Sha256::digest("old"));
    let new_hash = format!("{:x}", Sha256::digest("new"));
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: old_hash.clone(),
            imported_thread_id: thread_id,
            connector_names: vec!["Gmail".to_string()],
            title: Some("Original title".to_string()),
        }],
    )
    .expect("record initial import");
    std::fs::write(&source_path, "new").expect("updated source");
    let ledger_before = ledger_bytes(&codex_home);
    for (candidate_thread_id, expected_hash, candidate_hash) in [
        (ThreadId::new(), old_hash.as_str(), new_hash.as_str()),
        (thread_id, "stale", new_hash.as_str()),
        (thread_id, old_hash.as_str(), "not-the-source-hash"),
    ] {
        assert!(
            begin_existing_session_append(
                &codex_home,
                &source_path,
                candidate_thread_id,
                expected_hash,
                candidate_hash,
                super::ExistingSessionAppendPosition {
                    current_model_item_count: 1,
                    legacy_pending_model_item_count: Some(1),
                },
            )
            .expect("rejected checkpoint")
            .is_none()
        );
    }
    assert!(
        !checkpoint_existing_session_import(
            &codex_home,
            &source_path,
            thread_id,
            &old_hash,
            &new_hash,
        )
        .expect("checkpoint without append intent")
    );
    assert_eq!(ledger_bytes(&codex_home), ledger_before);

    assert_eq!(
        begin_existing_session_append(
            &codex_home,
            &source_path,
            thread_id,
            &old_hash,
            &new_hash,
            super::ExistingSessionAppendPosition {
                current_model_item_count: 3,
                legacy_pending_model_item_count: Some(3),
            },
        )
        .expect("begin append"),
        Some(3)
    );
    let pending_ledger = super::load_import_ledger(&codex_home).expect("pending ledger");
    let [pending_record] = pending_ledger.records.as_slice() else {
        panic!("append intent must preserve one record");
    };
    assert_eq!(pending_record.content_sha256, old_hash);
    assert_eq!(
        pending_record.pending_content_sha256.as_deref(),
        Some(new_hash.as_str())
    );
    assert_eq!(pending_record.pending_target_model_item_count, Some(3));
    assert!(
        !pending_ledger
            .contains_current_source(&source_path)
            .expect("pending append is not complete")
    );
    let pending_ledger_bytes = ledger_bytes(&codex_home);
    assert_eq!(
        begin_existing_session_append(
            &codex_home,
            &source_path,
            thread_id,
            &old_hash,
            &new_hash,
            super::ExistingSessionAppendPosition {
                current_model_item_count: 9,
                legacy_pending_model_item_count: Some(9),
            },
        )
        .expect("resume pending append"),
        Some(3)
    );
    assert_eq!(ledger_bytes(&codex_home), pending_ledger_bytes);

    assert!(
        checkpoint_existing_session_import(
            &codex_home,
            &source_path,
            thread_id,
            &old_hash,
            &new_hash,
        )
        .expect("checkpoint append")
    );
    let ledger = super::load_import_ledger(&codex_home).expect("updated ledger");
    let [record] = ledger.records.as_slice() else {
        panic!("checkpoint must preserve one record");
    };
    assert_eq!(
        record,
        &super::ImportedExternalAgentSessionRecord {
            source_path: source_path.clone(),
            content_sha256: new_hash,
            initial_import_pending: false,
            pending_content_sha256: None,
            pending_target_model_item_count: None,
            imported_thread_id: thread_id,
            imported_at: record.imported_at,
            source_modified_at: super::session_modified_at(&source_path).expect("source mtime"),
            connector_names: vec!["Gmail".to_string()],
            title: Some("Original title".to_string()),
        }
    );
}

#[test]
fn predecessor_pending_append_recovers_missing_model_item_count() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "old").expect("initial source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let thread_id = ThreadId::new();
    let old_hash = format!("{:x}", Sha256::digest("old"));
    let new_hash = format!("{:x}", Sha256::digest("new"));
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: old_hash.clone(),
            imported_thread_id: thread_id,
            connector_names: Vec::new(),
            title: None,
        }],
    )
    .expect("record initial import");
    std::fs::write(&source_path, "new").expect("updated source");
    let mut predecessor: serde_json::Value =
        serde_json::from_slice(&ledger_bytes(&codex_home)).expect("ledger JSON");
    predecessor["records"][0]["pending_content_sha256"] =
        serde_json::Value::String(new_hash.clone());
    predecessor["records"][0]
        .as_object_mut()
        .expect("ledger record")
        .remove("pending_target_model_item_count");
    std::fs::write(
        super::import_ledger_path(&codex_home),
        serde_json::to_vec_pretty(&predecessor).expect("serialize predecessor ledger"),
    )
    .expect("write predecessor ledger");

    assert_eq!(
        begin_existing_session_append(
            &codex_home,
            &source_path,
            thread_id,
            &old_hash,
            &new_hash,
            super::ExistingSessionAppendPosition {
                current_model_item_count: 2,
                legacy_pending_model_item_count: Some(1),
            },
        )
        .expect("resume predecessor append"),
        Some(1)
    );
    let ledger = super::load_import_ledger(&codex_home).expect("upgraded ledger");
    assert_eq!(ledger.records[0].pending_target_model_item_count, Some(1));
}

#[test]
fn newer_source_preserves_the_pending_transaction_base() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "v1").expect("initial source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let thread_id = ThreadId::new();
    let hashes = ["v1", "v2", "v3"].map(|value| format!("{:x}", Sha256::digest(value)));
    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: hashes[0].clone(),
            imported_thread_id: thread_id,
            connector_names: Vec::new(),
            title: None,
        }],
    )
    .expect("record initial import");
    std::fs::write(&source_path, "v2").expect("second source");
    assert_eq!(
        begin_existing_session_append(
            &codex_home,
            &source_path,
            thread_id,
            &hashes[0],
            &hashes[1],
            super::ExistingSessionAppendPosition {
                current_model_item_count: 3,
                legacy_pending_model_item_count: Some(3),
            },
        )
        .expect("begin second source"),
        Some(3)
    );

    std::fs::write(&source_path, "v3").expect("third source");
    assert_eq!(
        begin_existing_session_append(
            &codex_home,
            &source_path,
            thread_id,
            &hashes[0],
            &hashes[2],
            super::ExistingSessionAppendPosition {
                current_model_item_count: 4,
                legacy_pending_model_item_count: Some(3),
            },
        )
        .expect("advance pending source"),
        Some(3)
    );
    let ledger = super::load_import_ledger(&codex_home).expect("pending ledger");
    assert_eq!(
        (
            ledger.records[0].pending_content_sha256.as_deref(),
            ledger.records[0].pending_target_model_item_count,
        ),
        (Some(hashes[2].as_str()), Some(3))
    );
}

#[test]
fn concurrent_first_imports_share_one_reserved_thread() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "source").expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let source_hash = format!("{:x}", Sha256::digest("source"));
    let first_candidate = ThreadId::new();
    let second_candidate = ThreadId::new();

    let first = reserve_session_import(&codex_home, &source_path, &source_hash, first_candidate)
        .expect("first reservation");
    let (first_thread_id, first_reservation) = match first {
        super::SessionImportSourceMapping::New {
            imported_thread_id,
            reservation,
        } => (imported_thread_id, reservation),
        mapping => panic!("unexpected first mapping: {mapping:?}"),
    };
    let second = reserve_session_import(&codex_home, &source_path, &source_hash, second_candidate)
        .expect("second reservation");

    assert_eq!(first_thread_id, first_candidate);
    assert!(!first_reservation.replaces_incomplete_thread());
    assert!(matches!(
        second,
        super::SessionImportSourceMapping::Ambiguous
    ));
    let ledger = super::load_import_ledger(&codex_home).expect("reserved ledger");
    assert!(ledger.records.is_empty());
    assert_eq!(ledger.pending_initial_imports.len(), 1);
    assert_eq!(
        ledger.pending_initial_imports[0].imported_thread_id,
        first_candidate
    );
    assert!(
        !ledger
            .contains_current_source(&source_path)
            .expect("reservation is not completion")
    );

    let predecessor_view: serde_json::Value =
        serde_json::from_slice(&ledger_bytes(&codex_home)).expect("ledger JSON");
    assert_eq!(predecessor_view["records"], serde_json::json!([]));

    drop(first_reservation);
    let recovered =
        reserve_session_import(&codex_home, &source_path, &source_hash, second_candidate)
            .expect("recover reservation");
    match recovered {
        super::SessionImportSourceMapping::New {
            imported_thread_id,
            reservation,
        } => {
            assert_eq!(imported_thread_id, first_candidate);
            assert!(reservation.replaces_incomplete_thread());
        }
        mapping => panic!("unexpected recovered mapping: {mapping:?}"),
    }
}

#[test]
fn interrupted_first_import_recovers_latest_source_under_the_reserved_thread() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "v1").expect("initial source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let first_hash = format!("{:x}", Sha256::digest("v1"));
    let first_candidate = ThreadId::new();
    let second_candidate = ThreadId::new();

    let first = reserve_session_import(&codex_home, &source_path, &first_hash, first_candidate)
        .expect("first reservation");
    let first_reservation = match first {
        super::SessionImportSourceMapping::New { reservation, .. } => reservation,
        mapping => panic!("unexpected first mapping: {mapping:?}"),
    };
    std::fs::write(&source_path, "v2").expect("updated source");
    let second_hash = format!("{:x}", Sha256::digest("v2"));
    assert!(matches!(
        reserve_session_import(&codex_home, &source_path, &second_hash, second_candidate)
            .expect("contending reservation"),
        super::SessionImportSourceMapping::Ambiguous
    ));

    drop(first_reservation);
    let recovered =
        reserve_session_import(&codex_home, &source_path, &second_hash, second_candidate)
            .expect("recover updated source");
    match recovered {
        super::SessionImportSourceMapping::New {
            imported_thread_id,
            reservation,
        } => {
            assert_eq!(imported_thread_id, first_candidate);
            assert!(reservation.replaces_incomplete_thread());
        }
        mapping => panic!("unexpected recovered mapping: {mapping:?}"),
    }
    let ledger = super::load_import_ledger(&codex_home).expect("recovered ledger");
    assert_eq!(ledger.pending_initial_imports.len(), 1);
    assert_eq!(
        ledger.pending_initial_imports[0].content_sha256,
        second_hash
    );
}

#[test]
fn predecessor_pending_record_is_normalized_without_looking_completed() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "source").expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let source_hash = format!("{:x}", Sha256::digest("source"));
    let reserved_thread_id = ThreadId::new();
    super::save_import_ledger_unlocked(
        &codex_home,
        &super::ImportedExternalAgentSessionLedger {
            records: vec![super::ImportedExternalAgentSessionRecord {
                source_path: source_path.clone(),
                content_sha256: source_hash.clone(),
                initial_import_pending: true,
                pending_content_sha256: None,
                pending_target_model_item_count: None,
                imported_thread_id: reserved_thread_id,
                imported_at: super::now_unix_seconds(),
                source_modified_at: super::session_modified_at(&source_path).expect("source mtime"),
                connector_names: Vec::new(),
                title: None,
            }],
            ..Default::default()
        },
    )
    .expect("predecessor ledger");
    assert!(
        !super::load_import_ledger(&codex_home)
            .expect("predecessor ledger read")
            .contains_current_source(&source_path)
            .expect("pending predecessor is not complete")
    );

    let mapping = reserve_session_import(&codex_home, &source_path, &source_hash, ThreadId::new())
        .expect("recover predecessor reservation");
    match mapping {
        super::SessionImportSourceMapping::New {
            imported_thread_id,
            reservation,
        } => {
            assert_eq!(imported_thread_id, reserved_thread_id);
            assert!(reservation.replaces_incomplete_thread());
        }
        mapping => panic!("unexpected predecessor mapping: {mapping:?}"),
    }
    let ledger = super::load_import_ledger(&codex_home).expect("normalized ledger");
    assert!(ledger.records.is_empty());
    assert_eq!(ledger.pending_initial_imports.len(), 1);
}

#[test]
fn completion_does_not_pair_an_old_hash_with_a_new_source_mtime() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "old").expect("initial source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");
    let old_hash = format!("{:x}", Sha256::digest("old"));
    std::fs::write(&source_path, "new").expect("updated source");

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: old_hash,
            imported_thread_id: ThreadId::new(),
            connector_names: Vec::new(),
            title: None,
        }],
    )
    .expect("record raced completion");

    let ledger = super::load_import_ledger(&codex_home).expect("ledger");
    assert_eq!(ledger.records[0].source_modified_at, None);
    assert!(
        !ledger
            .contains_current_source(&source_path)
            .expect("changed source must remain detectable")
    );
}

#[test]
fn connector_candidates_accumulate_imports_for_each_source() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let first_source = root.path().join("first.jsonl");
    let second_source = root.path().join("second.jsonl");

    record_completed_session_imports(
        &codex_home,
        vec![
            CompletedExternalAgentSessionImport {
                source_path: first_source.clone(),
                source_content_sha256: "first-version".to_string(),
                imported_thread_id: ThreadId::new(),
                connector_names: vec!["Gmail".to_string()],
                title: None,
            },
            CompletedExternalAgentSessionImport {
                source_path: first_source,
                source_content_sha256: "second-version".to_string(),
                imported_thread_id: ThreadId::new(),
                connector_names: vec!["Slack".to_string()],
                title: None,
            },
            CompletedExternalAgentSessionImport {
                source_path: second_source,
                source_content_sha256: "only-version".to_string(),
                imported_thread_id: ThreadId::new(),
                connector_names: vec!["Gmail".to_string(), "Slack".to_string()],
                title: None,
            },
        ],
    )
    .expect("record imports");

    assert_eq!(
        read_imported_connector_candidates(&codex_home).expect("read connector candidates"),
        vec![
            ImportedConnectorCandidate {
                name: "Gmail".to_string(),
                session_count: 2,
            },
            ImportedConnectorCandidate {
                name: "Slack".to_string(),
                session_count: 2,
            },
        ]
    );
}

#[test]
fn connector_candidates_accumulate_detected_and_imported_session_clues() {
    let root = TempDir::new().expect("tempdir");
    let codex_home = root.path().join("codex-home");
    let source_path = root.path().join("session.jsonl");
    std::fs::write(&source_path, "session contents").expect("source");
    let source_path = std::fs::canonicalize(source_path).expect("canonical source");

    record_detected_session_connectors(
        &codex_home,
        BTreeMap::from([(source_path.clone(), vec!["Figma".to_string()])]),
    )
    .expect("record detected connectors");
    assert_eq!(
        read_imported_connector_candidates(&codex_home).expect("read detected connectors"),
        vec![ImportedConnectorCandidate {
            name: "Figma".to_string(),
            session_count: 1,
        }]
    );

    record_completed_session_imports(
        &codex_home,
        vec![CompletedExternalAgentSessionImport {
            source_path: source_path.clone(),
            source_content_sha256: "content-sha".to_string(),
            imported_thread_id: ThreadId::new(),
            connector_names: vec!["Slack".to_string()],
            title: None,
        }],
    )
    .expect("record completed import");
    assert_eq!(
        read_imported_connector_candidates(&codex_home).expect("read imported connectors"),
        vec![
            ImportedConnectorCandidate {
                name: "Figma".to_string(),
                session_count: 1,
            },
            ImportedConnectorCandidate {
                name: "Slack".to_string(),
                session_count: 1,
            },
        ]
    );

    append_imported_session_connector_names(
        &codex_home,
        BTreeMap::from([(source_path, vec!["Gmail".to_string(), "FIGMA".to_string()])]),
    )
    .expect("append imported connector names");
    assert_eq!(
        read_imported_connector_candidates(&codex_home).expect("read appended connectors"),
        vec![
            ImportedConnectorCandidate {
                name: "Figma".to_string(),
                session_count: 1,
            },
            ImportedConnectorCandidate {
                name: "Gmail".to_string(),
                session_count: 1,
            },
            ImportedConnectorCandidate {
                name: "Slack".to_string(),
                session_count: 1,
            },
        ]
    );
}

fn ledger_bytes(codex_home: &Path) -> Vec<u8> {
    std::fs::read(super::import_ledger_path(codex_home)).expect("ledger")
}
