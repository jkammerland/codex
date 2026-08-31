use std::collections::HashMap;
use std::path::Path;

use super::export::EXTERNAL_SESSION_IMPORTED_MARKER;
use super::export::bounded_external_response_item;
use super::ledger::ExistingSessionAppendPosition;
use super::ledger::begin_existing_session_append;
use super::ledger::checkpoint_existing_session_import;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutModelResponseProjection;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Semaphore;

/// A changed external session and the existing native thread it may extend.
pub struct ExistingSessionAppend<'a> {
    pub source_path: &'a Path,
    pub source_content_sha256: &'a str,
    pub expected_source_content_sha256: &'a str,
    pub thread_id: ThreadId,
    pub source_items: &'a [RolloutItem],
}

/// Appends an exact missing suffix to one cold imported thread and checkpoints the source.
///
/// Any unavailable, active, archived, malformed, or diverged destination fails closed.
pub async fn append_existing_session(
    codex_home: &Path,
    checkpoint_permits: &Semaphore,
    thread_manager: &ThreadManager,
    thread_store: &dyn ThreadStore,
    request: ExistingSessionAppend<'_>,
) -> bool {
    let ExistingSessionAppend {
        source_path,
        source_content_sha256,
        expected_source_content_sha256,
        thread_id,
        source_items,
    } = request;
    let Ok(_checkpoint_permit) = checkpoint_permits.acquire().await else {
        return false;
    };
    let Ok(mut thread) = thread_store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
    else {
        return false;
    };
    if thread.thread_id != thread_id || thread.archived_at.is_some() {
        return false;
    }
    let Some(history) = thread.history.take() else {
        return false;
    };
    if history.thread_id != thread_id || thread_manager.get_thread(thread_id).await.is_ok() {
        return false;
    }
    let Some(metadata) = persistence_metadata(&history.items, thread_id) else {
        return false;
    };
    let Some(rollout_path) = thread.rollout_path else {
        return false;
    };
    let Some(target_model_item_count) =
        history_model_items(&history.items).map(|items| items.len())
    else {
        return false;
    };
    let legacy_pending_model_item_count = legacy_import_base_model_item_count(&history.items);
    if plan_existing_session_append(source_items, &history.items).is_none() {
        return false;
    }

    let codex_home_buf = codex_home.to_path_buf();
    let source_path_buf = source_path.to_path_buf();
    let expected_hash = expected_source_content_sha256.to_string();
    let source_hash = source_content_sha256.to_string();
    let append_base_model_item_count = match tokio::task::spawn_blocking(move || {
        begin_existing_session_append(
            &codex_home_buf,
            &source_path_buf,
            thread_id,
            &expected_hash,
            &source_hash,
            ExistingSessionAppendPosition {
                current_model_item_count: target_model_item_count,
                legacy_pending_model_item_count,
            },
        )
    })
    .await
    {
        Ok(Ok(Some(append_base_model_item_count))) => append_base_model_item_count,
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return false,
    };
    let Some(initial_plan) = plan_existing_session_append_from_base(
        source_items,
        &history.items,
        append_base_model_item_count,
    ) else {
        return false;
    };

    if matches!(initial_plan, ExistingSessionAppendPlan::AlreadyComplete) {
        let codex_home = codex_home.to_path_buf();
        let source_path = source_path.to_path_buf();
        let expected_source_content_sha256 = expected_source_content_sha256.to_string();
        let source_content_sha256 = source_content_sha256.to_string();
        return matches!(
            tokio::task::spawn_blocking(move || {
                checkpoint_existing_session_import(
                    &codex_home,
                    &source_path,
                    thread_id,
                    &expected_source_content_sha256,
                    &source_content_sha256,
                )
            })
            .await,
            Ok(Ok(true))
        );
    }
    if thread_store
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout_path),
            history: None,
            include_archived: false,
            metadata,
        })
        .await
        .is_err()
    {
        return false;
    }

    let fresh_plan = match thread_store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
    {
        Ok(mut thread) if thread.thread_id == thread_id && thread.archived_at.is_none() => thread
            .history
            .take()
            .filter(|history| history.thread_id == thread_id)
            .filter(|history| persistence_metadata(&history.items, thread_id).is_some())
            .and_then(|history| {
                plan_existing_session_append_from_base(
                    source_items,
                    &history.items,
                    append_base_model_item_count,
                )
            }),
        Ok(_) | Err(_) => None,
    };
    let Some(fresh_plan) = fresh_plan else {
        let _ = thread_store.discard_thread(thread_id).await;
        return false;
    };
    match fresh_plan {
        ExistingSessionAppendPlan::Append(items) => {
            if thread_store
                .append_items(AppendThreadItemsParams { thread_id, items })
                .await
                .is_err()
            {
                let _ = thread_store.discard_thread(thread_id).await;
                return false;
            }
            if thread_store.shutdown_thread(thread_id).await.is_err() {
                let _ = thread_store.discard_thread(thread_id).await;
                return false;
            }
        }
        ExistingSessionAppendPlan::AlreadyComplete => {
            if thread_store.discard_thread(thread_id).await.is_err() {
                return false;
            }
        }
    }
    let target_matches_source = match thread_store
        .read_thread(ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
    {
        Ok(mut thread) if thread.thread_id == thread_id && thread.archived_at.is_none() => thread
            .history
            .take()
            .filter(|history| history.thread_id == thread_id)
            .filter(|history| persistence_metadata(&history.items, thread_id).is_some())
            .is_some_and(|history| {
                model_transcripts_match(source_items, &history.items)
                    && matches!(
                        plan_existing_session_append_from_base(
                            source_items,
                            &history.items,
                            append_base_model_item_count,
                        ),
                        Some(ExistingSessionAppendPlan::AlreadyComplete)
                    )
            }),
        Ok(_) | Err(_) => false,
    };
    if !target_matches_source {
        return false;
    }

    let codex_home = codex_home.to_path_buf();
    let source_path = source_path.to_path_buf();
    let expected_source_content_sha256 = expected_source_content_sha256.to_string();
    let source_content_sha256 = source_content_sha256.to_string();
    let checkpointed = matches!(
        tokio::task::spawn_blocking(move || {
            checkpoint_existing_session_import(
                &codex_home,
                &source_path,
                thread_id,
                &expected_source_content_sha256,
                &source_content_sha256,
            )
        })
        .await,
        Ok(Ok(true))
    );
    checkpointed
}

struct SourceModelItem {
    response_item: ResponseItem,
    append_start_index: usize,
}

enum ExistingSessionAppendPlan {
    AlreadyComplete,
    Append(Vec<RolloutItem>),
}

fn plan_existing_session_append(
    source_items: &[RolloutItem],
    history_items: &[RolloutItem],
) -> Option<ExistingSessionAppendPlan> {
    if model_transcripts_match_exact(source_items, history_items) {
        Some(ExistingSessionAppendPlan::AlreadyComplete)
    } else {
        plan_append(source_items, history_items).map(ExistingSessionAppendPlan::Append)
    }
}

fn plan_existing_session_append_from_base(
    source_items: &[RolloutItem],
    history_items: &[RolloutItem],
    append_base_model_item_count: usize,
) -> Option<ExistingSessionAppendPlan> {
    let source = source_model_items(source_items)?;
    let history = history_model_items(history_items)?;
    if append_base_model_item_count > history.len()
        || history.len() > source.len()
        || history
            .iter()
            .zip(&source)
            .any(|(history, source)| !model_items_match(history, &source.response_item))
    {
        return None;
    }
    if append_base_model_item_count == source.len() {
        return (history.len() == source.len()
            && history
                .iter()
                .zip(&source)
                .all(|(history, source)| history == &source.response_item))
        .then_some(ExistingSessionAppendPlan::AlreadyComplete);
    }

    let append_start_index = source.get(append_base_model_item_count)?.append_start_index;
    let transaction = source_items[append_start_index..]
        .iter()
        .filter(|item| !is_import_marker(item))
        .cloned()
        .collect::<Vec<_>>();
    let expected_persisted_model_items = history.len().saturating_sub(append_base_model_item_count);
    let persisted_prefix_len = matching_transaction_prefix_len(
        &transaction,
        history_items,
        expected_persisted_model_items,
    )?;

    if persisted_prefix_len == transaction.len() {
        Some(ExistingSessionAppendPlan::AlreadyComplete)
    } else {
        Some(ExistingSessionAppendPlan::Append(
            transaction[persisted_prefix_len..].to_vec(),
        ))
    }
}

fn matching_transaction_prefix_len(
    transaction: &[RolloutItem],
    history_items: &[RolloutItem],
    expected_model_item_count: usize,
) -> Option<usize> {
    let mut projected_counts = Vec::with_capacity(transaction.len().saturating_add(1));
    projected_counts.push(0usize);
    let mut projection = RolloutModelResponseProjection::default();
    let mut fingerprints = Vec::with_capacity(transaction.len());
    let mut original_prefix_by_fingerprint_count = HashMap::new();
    if expected_model_item_count == 0 {
        original_prefix_by_fingerprint_count.insert(0usize, 0usize);
    }
    for (index, item) in transaction.iter().enumerate() {
        projected_counts.push(
            projected_counts
                .last()
                .copied()?
                .saturating_add(projection.project(item).len()),
        );
        if let Some(fingerprint) = rollout_item_recovery_fingerprint(item)? {
            fingerprints.push(fingerprint);
        }
        if projected_counts.last().copied() == Some(expected_model_item_count) {
            original_prefix_by_fingerprint_count.insert(fingerprints.len(), index + 1);
        }
    }

    let mut failure = vec![0usize; fingerprints.len()];
    for index in 1..fingerprints.len() {
        let mut prefix_len = failure[index - 1];
        while prefix_len > 0 && fingerprints[index] != fingerprints[prefix_len] {
            prefix_len = failure[prefix_len - 1];
        }
        if fingerprints[index] == fingerprints[prefix_len] {
            prefix_len += 1;
        }
        failure[index] = prefix_len;
    }

    let mut matched = 0usize;
    for item in history_items {
        let Some(fingerprint) = rollout_item_recovery_fingerprint(item)? else {
            continue;
        };
        while matched > 0 && (matched == fingerprints.len() || fingerprint != fingerprints[matched])
        {
            matched = failure[matched - 1];
        }
        if matched < fingerprints.len() && fingerprint == fingerprints[matched] {
            matched += 1;
        }
    }

    loop {
        if let Some(original_prefix_len) = original_prefix_by_fingerprint_count.get(&matched) {
            return Some(*original_prefix_len);
        }
        if matched == 0 {
            return None;
        }
        matched = failure[matched - 1];
    }
}

fn rollout_item_recovery_fingerprint(item: &RolloutItem) -> Option<Option<[u8; 32]>> {
    if matches!(item, RolloutItem::EventMsg(EventMsg::TokenCount(_))) {
        return Some(None);
    }
    let mut normalized = item.clone();
    if let RolloutItem::EventMsg(EventMsg::TurnComplete(completed)) = &mut normalized {
        completed.completed_at = None;
    }
    let serialized = serde_json::to_vec(&normalized).ok()?;
    Some(Some(Sha256::digest(serialized).into()))
}

/// Returns the nonempty source suffix that can be safely appended to `history_items`.
///
/// The destination's complete model-visible transcript must be an exact prefix of the
/// source transcript. Rollout metadata does not participate in transcript identity.
fn plan_append(
    source_items: &[RolloutItem],
    history_items: &[RolloutItem],
) -> Option<Vec<RolloutItem>> {
    let source = source_model_items(source_items)?;
    let history = history_model_items(history_items)?;
    if history.len() >= source.len()
        || history
            .iter()
            .zip(&source)
            .any(|(history, source)| !model_items_match(history, &source.response_item))
    {
        return None;
    }

    let append_start_index = source.get(history.len())?.append_start_index;
    let suffix = source_items[append_start_index..]
        .iter()
        .filter(|item| !is_import_marker(item))
        .cloned()
        .collect::<Vec<_>>();
    let mut model_response_projection = RolloutModelResponseProjection::default();
    suffix
        .iter()
        .any(|item| !model_response_projection.project(item).is_empty())
        .then_some(suffix)
}

/// Returns whether source and destination contain the same complete model-visible transcript.
///
/// Rollout metadata does not participate in transcript identity. Unsupported history shapes fail
/// closed.
fn model_transcripts_match(source_items: &[RolloutItem], history_items: &[RolloutItem]) -> bool {
    let Some(source) = source_model_items(source_items) else {
        return false;
    };
    let Some(history) = history_model_items(history_items) else {
        return false;
    };
    history.len() == source.len()
        && history
            .iter()
            .zip(&source)
            .all(|(history, source)| model_items_match(history, &source.response_item))
}

fn model_transcripts_match_exact(
    source_items: &[RolloutItem],
    history_items: &[RolloutItem],
) -> bool {
    let Some(source) = source_model_items(source_items) else {
        return false;
    };
    let Some(history) = history_model_items(history_items) else {
        return false;
    };
    history.len() == source.len()
        && history
            .iter()
            .zip(source)
            .all(|(history, source)| history == &source.response_item)
}

fn model_items_match(history: &ResponseItem, source: &ResponseItem) -> bool {
    history == source
        || bounded_external_response_item(history)
            .as_ref()
            .is_some_and(|bounded_history| bounded_history == source)
}

fn source_model_items(items: &[RolloutItem]) -> Option<Vec<SourceModelItem>> {
    let mut model_items = Vec::new();
    let mut append_start_index = None;
    let mut claim_append_start_indices = HashMap::new();
    let mut model_response_projection = RolloutModelResponseProjection::default();
    for (index, item) in items.iter().enumerate() {
        if let RolloutItem::DurableInterAgentCommunicationClaimed {
            communication_id,
            communication: Some(_),
            delivered_response: None,
            ..
        } = item
        {
            claim_append_start_indices
                .entry(communication_id.clone())
                .or_insert(index);
        }
        if matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnStarted(_))
                | RolloutItem::DurableInterAgentCommunicationClaimed {
                    communication: Some(_),
                    delivered_response: None,
                    ..
                }
        ) {
            append_start_index.get_or_insert(index);
        }
        let projected = model_response_projection.project(item);
        if !projected.is_empty() {
            let projected_append_start_index = append_start_index.take().unwrap_or(index);
            for response_item in projected {
                let response_append_start_index = response_item
                    .item
                    .id()
                    .and_then(|id| claim_append_start_indices.remove(id))
                    .unwrap_or(projected_append_start_index);
                model_items.push(SourceModelItem {
                    response_item: response_item.item.clone(),
                    append_start_index: response_append_start_index,
                });
            }
        }
        match item {
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::DurableInterAgentCommunication { .. }
            | RolloutItem::DurableInterAgentCommunicationClaimed { .. }
            | RolloutItem::RealtimeItem(_) => {}
            RolloutItem::ResponseItem(_) => {}
            RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {}
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                append_start_index.get_or_insert(index);
            }
            RolloutItem::EventMsg(EventMsg::AgentMessage(event))
                if event.message != EXTERNAL_SESSION_IMPORTED_MARKER =>
            {
                append_start_index = Some(index);
            }
            RolloutItem::EventMsg(
                EventMsg::ContextCompacted(_) | EventMsg::ThreadRolledBack(_),
            )
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::WorldState(_) => return None,
            RolloutItem::EventMsg(_) => {}
        }
    }
    Some(model_items)
}

fn history_model_items(items: &[RolloutItem]) -> Option<Vec<ResponseItem>> {
    let mut model_items = Vec::new();
    let mut model_response_projection = RolloutModelResponseProjection::default();
    for item in items {
        for response_item in model_response_projection.project(item) {
            model_items.push(response_item.item.clone());
        }
        match item {
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TerminalAgentStatus { .. }
            | RolloutItem::DurableInterAgentCommunication { .. }
            | RolloutItem::DurableInterAgentCommunicationClaimed { .. }
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::SecurityRiskScore(_) => {}
            RolloutItem::ResponseItem(_) => {}
            RolloutItem::EventMsg(
                EventMsg::ContextCompacted(_) | EventMsg::ThreadRolledBack(_),
            )
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_) => return None,
            RolloutItem::EventMsg(_) => {}
        }
    }
    Some(model_items)
}

fn is_import_marker(item: &RolloutItem) -> bool {
    matches!(
        item,
        RolloutItem::EventMsg(EventMsg::AgentMessage(event))
            if event.message == EXTERNAL_SESSION_IMPORTED_MARKER
    )
}

fn legacy_import_base_model_item_count(history_items: &[RolloutItem]) -> Option<usize> {
    let mut marker_positions = history_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| is_import_marker(item).then_some(index));
    let marker_position = marker_positions.next()?;
    if marker_positions.next().is_some() {
        return None;
    }
    history_model_items(&history_items[..marker_position]).map(|items| items.len())
}

fn persistence_metadata(
    history_items: &[RolloutItem],
    thread_id: ThreadId,
) -> Option<ThreadPersistenceMetadata> {
    let RolloutItem::SessionMeta(first_meta_line) = history_items.first()? else {
        return None;
    };
    if first_meta_line.meta.id != thread_id {
        return None;
    }
    let meta = history_items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == thread_id => {
            Some(&meta_line.meta)
        }
        _ => None,
    })?;
    if meta.cwd.as_os_str().is_empty()
        || meta
            .model_provider
            .as_deref()
            .is_none_or(|provider| provider.trim().is_empty())
    {
        return None;
    }
    let memory_mode = match meta.memory_mode.as_deref() {
        None | Some("enabled") => ThreadMemoryMode::Enabled,
        Some("disabled") => ThreadMemoryMode::Disabled,
        Some(_) => return None,
    };
    Some(ThreadPersistenceMetadata {
        cwd: Some(meta.cwd.clone()),
        model_provider: meta.model_provider.clone()?,
        memory_mode,
    })
}

#[cfg(test)]
#[path = "append_tests.rs"]
mod tests;
