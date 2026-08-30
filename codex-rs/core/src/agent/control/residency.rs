use super::AgentControl;
use crate::agent::AgentStatus;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const RESIDENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ThreadId>,
    pending_slots: usize,
}

struct PoppedResident<'a> {
    residency: &'a V2Residency,
    thread_id: ThreadId,
    restore_on_drop: bool,
}

impl PoppedResident<'_> {
    fn discard(mut self) {
        self.restore_on_drop = false;
    }
}

impl Drop for PoppedResident<'_> {
    fn drop(&mut self) {
        if self.restore_on_drop {
            self.residency.touch(self.thread_id);
        }
    }
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

pub(super) struct DeferredV2ResidencyRelease {
    residency: Arc<V2Residency>,
}

impl Drop for DeferredV2ResidencyRelease {
    fn drop(&mut self) {
        self.residency.release_pending_slot();
    }
}

impl V2ResidencySlot {
    pub(super) fn commit(mut self, thread_id: ThreadId) {
        self.residency.commit_slot(thread_id);
        self.active = false;
    }
}

impl Drop for V2ResidencySlot {
    fn drop(&mut self) {
        if self.active {
            self.residency.release_pending_slot();
        }
    }
}

impl AgentControl {
    pub(super) async fn reserve_v2_residency_slot(
        &self,
        state: &Arc<ThreadManagerState>,
        config: &Config,
        protected_thread_id: Option<ThreadId>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<V2ResidencySlot> {
        let capacity = config
            .effective_agent_max_threads(MultiAgentVersion::V2)
            .unwrap_or(usize::MAX);
        Arc::clone(&self.v2_residency)
            .reserve_slot(state, capacity, protected_thread_id, cancellation_token)
            .await
    }

    pub(super) async fn touch_loaded_v2_residency(
        &self,
        state: &Arc<ThreadManagerState>,
        thread_id: ThreadId,
    ) {
        if let Ok(thread) = state.get_thread(thread_id).await
            && is_resident_candidate(thread.as_ref())
        {
            self.v2_residency.touch(thread_id);
        }
    }

    pub(super) fn forget_v2_residency(&self, thread_id: ThreadId) {
        self.v2_residency.remove(thread_id);
    }

    pub(super) fn retain_unpublished_v2_residency_slot(
        &self,
        thread: &CodexThread,
    ) -> Option<DeferredV2ResidencyRelease> {
        is_resident_candidate(thread).then(|| {
            self.v2_residency.retain_pending_slot();
            DeferredV2ResidencyRelease {
                residency: Arc::clone(&self.v2_residency),
            }
        })
    }
}

impl V2Residency {
    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        capacity: usize,
        protected_thread_id: Option<ThreadId>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<V2ResidencySlot> {
        loop {
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::Interrupted);
            }
            if self.try_reserve_pending_slot(capacity) {
                return Ok(V2ResidencySlot {
                    residency: self,
                    active: true,
                });
            }
            if !self
                .try_unload_one_resident(manager, protected_thread_id, cancellation_token)
                .await?
            {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads: capacity,
                }));
            }
        }
    }

    fn try_reserve_pending_slot(&self, capacity: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.residents.len().saturating_add(state.pending_slots) >= capacity {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        &self,
        manager: &Arc<ThreadManagerState>,
        protected_thread_id: Option<ThreadId>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<bool> {
        let candidates_to_scan = self.resident_count();
        for _ in 0..candidates_to_scan {
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::Interrupted);
            }
            let Some(candidate_thread_id) = self.pop_lru_candidate(protected_thread_id) else {
                return Ok(false);
            };
            let popped_resident = PoppedResident {
                residency: self,
                thread_id: candidate_thread_id,
                restore_on_drop: true,
            };
            let candidate_thread = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::Interrupted),
                thread = manager.get_thread(candidate_thread_id) => thread,
            };
            let Some(candidate_thread) = candidate_thread
                .ok()
                .filter(|thread| is_resident_candidate(thread))
            else {
                popped_resident.discard();
                continue;
            };
            let delivery_permit = candidate_thread.session.agent_delivery_gate.clone();
            let Ok(_delivery_permit) = (tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::Interrupted),
                permit = delivery_permit.acquire_owned() => permit,
            }) else {
                continue;
            };
            let has_nonterminal_child = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::Interrupted),
                has_child = has_nonterminal_loaded_child(manager, candidate_thread_id) => has_child,
            };
            if has_nonterminal_child {
                continue;
            }
            let unloadable = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::Interrupted),
                unloadable = is_unloadable(candidate_thread.as_ref()) => unloadable,
            };
            if !unloadable {
                continue;
            }
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::Interrupted);
            }
            candidate_thread.ensure_rollout_materialized().await;
            // Once shutdown is submitted it must not be dropped on cancellation. The bounded
            // observer keeps a broken teardown from wedging unrelated spawn admission forever.
            let shutdown = tokio::time::timeout(
                RESIDENT_SHUTDOWN_TIMEOUT,
                candidate_thread.shutdown_and_wait(),
            )
            .await;
            if !matches!(shutdown, Ok(Ok(()))) {
                warn!(
                    "failed to shut down v2 resident thread before unloading {candidate_thread_id}: {shutdown:?}"
                );
                continue;
            }
            let environments = candidate_thread.environment_selections().await;
            candidate_thread
                .session
                .services
                .agent_control
                .state
                .save_evicted_environments(candidate_thread_id, environments);
            let _ = manager.remove_thread(&candidate_thread_id).await;
            popped_resident.discard();
            return Ok(true);
        }
        Ok(false)
    }

    fn resident_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .len()
    }

    fn pop_lru_candidate(&self, protected_thread_id: Option<ThreadId>) -> Option<ThreadId> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let candidates_to_scan = state.residents.len();
        for _ in 0..candidates_to_scan {
            let candidate_thread_id = state.residents.pop_front()?;
            if Some(candidate_thread_id) == protected_thread_id {
                state.residents.push_back(candidate_thread_id);
                continue;
            }
            return Some(candidate_thread_id);
        }
        None
    }

    fn touch(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        touch_resident(&mut state.residents, thread_id);
    }

    fn remove(&self, thread_id: ThreadId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .retain(|resident_thread_id| *resident_thread_id != thread_id);
    }

    fn commit_slot(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        touch_resident(&mut state.residents, thread_id);
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
    }

    fn retain_pending_slot(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_slots += 1;
    }
}

fn touch_resident(residents: &mut VecDeque<ThreadId>, thread_id: ThreadId) {
    residents.retain(|resident_thread_id| *resident_thread_id != thread_id);
    residents.push_back(thread_id);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn has_nonterminal_loaded_child(
    manager: &ThreadManagerState,
    parent_thread_id: ThreadId,
) -> bool {
    for (candidate_parent_thread_id, child_thread_id) in
        manager.list_live_thread_spawn_edges().await
    {
        if candidate_parent_thread_id != parent_thread_id {
            continue;
        }
        if let Ok(child) = manager.get_thread(child_thread_id).await
            && !crate::agent::status::is_final(&child.agent_status().await)
        {
            return true;
        }
    }
    false
}

async fn is_unloadable(thread: &CodexThread) -> bool {
    matches!(
        thread.agent_status().await,
        AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted
    ) && thread.session.active_turn.lock().await.is_none()
        && !thread.session.input_queue.has_pending_mailbox_items().await
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
