use super::LocalThreadStore;
use super::read_thread;
use super::thread_rollout_resolver;
use crate::LoadThreadHistoryParams;
use crate::StoredExecutionHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn load(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredExecutionHistory> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    }
    .ok_or(ThreadStoreError::ThreadNotFound {
        thread_id: params.thread_id,
    })?;
    Ok(StoredExecutionHistory {
        generation: resolved.rollout_id.to_string(),
        items: read_thread::load_history_items(resolved.path.as_path()).await?,
    })
}
